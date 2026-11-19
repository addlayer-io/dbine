//! Vectors (embeddings) in Elasticsearch, OpenSearch and Open Distro, and
//! the pieces of them the Solr driver shares.
//!
//! - **Types.** Elasticsearch `dense_vector` (`dims`, `element_type` float,
//!   byte, bit or bfloat16), `sparse_vector` and the multi-vector
//!   `rank_vectors` (8.18+: several vectors per document, so its cells keep
//!   their JSON and it has no ANN index); OpenSearch and Open Distro
//!   `knn_vector` (`dimension`, OpenSearch's `data_type` float, byte or
//!   binary). DBine spells a vector field's type with its dimension and
//!   element (`dense_vector(1536)`, `dense_vector(1536, byte)`,
//!   `knn_vector(768, binary)`), so describe, compare, clone and schema
//!   conversion keep them; the mapping takes them apart again
//!   ([`VecType::parse`]). A bare `dense_vector` (no dimension yet: the
//!   first document gives it) and the legacy `dims` column option still
//!   work.
//! - **Vector indexes.** The ANN index is part of the field's mapping. DBine
//!   shows it as a vector index ([`IndexDef::vector`]) named after the field:
//!   - Elasticsearch: `index: true` with `similarity` and `index_options`,
//!     whose `type` is the algorithm with the quantization in front
//!     (`int8_hnsw` = `hnsw` + `quantization: int8`), plus `m`,
//!     `ef_construction`, `confidence_interval`…
//!   - OpenSearch: `index.knn: true` and the field's `method` (`name`,
//!     `space_type`, `engine`, `parameters`), `mode`, `compression_level`,
//!     or a trained `model_id`.
//!   - Open Distro: `index.knn: true` and the index's `knn.space_type` and
//!     `knn.algo_param.*`, shared by all its vector fields (its k-NN plugin
//!     has no `method`).
//!
//!   A vector field without a vector index isn't indexed where the engine
//!   lets a field say so (Elasticsearch `index: false`); in an OpenSearch
//!   k-NN index every `knn_vector` is, with the engine's defaults, so a
//!   schema sync takes "no vector index" and those defaults as the same
//!   ([`same_index`]). Settings left empty are the engine's choice. The
//!   native settings also go in [`IndexDef::options`] so a schema compare
//!   sees them.
//! - **Cells.** A dense vector is the canonical JSON array text
//!   ([`dbine_driver::vector`]): byte vectors given as hex strings decode
//!   to their numbers, and bit (binary) vectors, stored as packed bytes,
//!   read as one 0 or 1 per dimension. Sparse vectors keep the engine's
//!   JSON.

use crate::json::{Obj, J};
use crate::Flavor;
use dbine_driver::vector::{self as v, metric, param};
use dbine_driver::{
    Error, IndexDef, Result, VectorElement, VectorIndex, VectorIndexSpec, VectorMeta,
    VectorParamSpec,
};
use std::collections::BTreeMap;

/// What a vector's numbers are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elem {
    Float,
    /// Elasticsearch 9.3+ `bfloat16`.
    Bf16,
    /// Signed bytes.
    Byte,
    /// Packed bits: `dims` is the number of bits.
    Bit,
}

/// A vector field's type.
#[derive(Debug, Clone, PartialEq)]
pub struct VecType {
    /// The engine's type: `dense_vector`, `knn_vector`, `sparse_vector`…
    pub base: String,
    pub dims: Option<u32>,
    pub elem: Elem,
    pub sparse: bool,
    /// Several vectors per value (`rank_vectors`): not a vector column in
    /// results, and no ANN index.
    pub multi: bool,
    /// How a result column names it (DBine's spelling with the dimension).
    pub type_name: String,
    /// The engine gives back the float32 it stores (Solr), in whatever
    /// text its Java prints: cells are that float32's shortest text.
    /// Elasticsearch and OpenSearch give back `_source` as it was sent.
    pub single: bool,
}

impl VecType {
    pub fn new(base: &str, dims: Option<u32>, elem: Elem) -> Self {
        let mut t = Self {
            base: base.to_string(),
            dims,
            elem,
            sparse: base == "sparse_vector",
            multi: base == "rank_vectors",
            type_name: String::new(),
            single: false,
        };
        t.type_name = t.spelling();
        t
    }

    /// DBine's spelling: `dense_vector(3)`, `dense_vector(4, byte)`,
    /// `knn_vector(16, binary)`, `sparse_vector`.
    pub fn spelling(&self) -> String {
        let elem = match (self.elem, self.base.as_str()) {
            (Elem::Float, _) => None,
            (Elem::Byte, _) => Some("byte"),
            (Elem::Bit, "knn_vector") => Some("binary"),
            (Elem::Bit, _) => Some("bit"),
            (Elem::Bf16, _) => Some("bfloat16"),
        };
        match (self.dims, elem) {
            (Some(d), Some(e)) => format!("{}({d}, {e})", self.base),
            (Some(d), None) => format!("{}({d})", self.base),
            (None, Some(e)) => format!("{}(, {e})", self.base),
            (None, None) => self.base.clone(),
        }
    }

    pub fn meta(&self) -> VectorMeta {
        let element = match self.elem {
            Elem::Float => VectorElement::F32,
            Elem::Bf16 => VectorElement::Bf16,
            Elem::Byte => VectorElement::I8,
            Elem::Bit => VectorElement::Bit,
        };
        if self.sparse {
            VectorMeta::sparse(self.dims, element)
        } else {
            VectorMeta::dense(self.dims, element)
        }
    }

    /// The vector type of a field's mapping (`{"type": "dense_vector",
    /// "dims": 3, …}`), or `None` when it isn't one.
    pub fn from_mapping(f: &J) -> Option<VecType> {
        let ty = f.get("type").and_then(J::as_str)?;
        if ty == "sparse_vector" {
            return Some(VecType::new(ty, None, Elem::Float));
        }
        if !matches!(ty, "dense_vector" | "knn_vector" | "rank_vectors") {
            return None;
        }
        let dims = ["dims", "dimension"]
            .iter()
            .find_map(|k| f.get(k))
            .and_then(|d| d.text().parse().ok());
        let elem = ["element_type", "data_type"]
            .iter()
            .find_map(|k| f.get(k))
            .map(|e| elem_of(&e.text()))
            .unwrap_or(Some(Elem::Float))?;
        Some(VecType::new(ty, dims, elem))
    }

    /// DBine's spelling read back (see [`VecType::spelling`]): `None` when
    /// `ty` isn't a vector type, `Err` when its arguments are wrong.
    pub fn parse(ty: &str) -> Option<Result<VecType>> {
        let ty = ty.trim();
        let (base, args) = match ty.split_once('(') {
            Some((b, rest)) => (b.trim(), Some(rest.strip_suffix(')').unwrap_or(rest))),
            None => (ty, None),
        };
        let base = base.to_ascii_lowercase();
        if !matches!(
            base.as_str(),
            "dense_vector" | "knn_vector" | "rank_vectors"
        ) {
            return None;
        }
        let bad = || {
            Error::Query(format!(
                "«{ty}»: un {base} se escribe {base}(dimensiones) o {base}(dimensiones, tipo de elemento)."
            ))
        };
        let Some(args) = args else {
            return Some(Ok(VecType::new(&base, None, Elem::Float)));
        };
        let mut parts = args.split(',').map(str::trim);
        let dims = match parts.next().filter(|d| !d.is_empty()) {
            None => None,
            Some(d) => match d.parse::<u32>() {
                Ok(n) if n > 0 => Some(n),
                _ => return Some(Err(bad())),
            },
        };
        let elem = match parts.next() {
            None => Elem::Float,
            Some(e) => match elem_of(e) {
                Some(e) => e,
                None => return Some(Err(bad())),
            },
        };
        if parts.next().is_some() {
            return Some(Err(bad()));
        }
        Some(Ok(VecType::new(&base, dims, elem)))
    }
}

/// An element type by any of its engines' names.
fn elem_of(e: &str) -> Option<Elem> {
    match e.trim().to_ascii_lowercase().as_str() {
        "" | "float" | "float32" => Some(Elem::Float),
        "byte" => Some(Elem::Byte),
        "bit" | "binary" => Some(Elem::Bit),
        "bfloat16" => Some(Elem::Bf16),
        _ => None,
    }
}

/// The bytes of a hex string (`"0a7f"`), or `None` when it isn't one.
fn hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A vector value of `_source` (a JSON array, or a hex string for byte and
/// bit vectors) as its numbers: one per dimension, 0 or 1 for bits.
/// `None` when it can't be read as one.
pub fn numbers(val: &J, t: &VecType) -> Option<Vec<f64>> {
    let bytes = match val {
        J::Arr(a) => {
            let nums: Option<Vec<f64>> = a
                .iter()
                .map(|n| match n {
                    J::Num(n) => n.as_f64().filter(|x| x.is_finite()),
                    _ => None,
                })
                .collect();
            let nums = nums?;
            if t.elem != Elem::Bit {
                return Some(nums);
            }
            // Packed bits: signed (or unsigned) bytes.
            nums.iter()
                .map(|&x| (-128.0..=255.0).contains(&x).then_some(x as i64 as u8))
                .collect::<Option<Vec<u8>>>()?
        }
        J::Str(s) if matches!(t.elem, Elem::Byte | Elem::Bit) => hex(s)?,
        _ => return None,
    };
    Some(match t.elem {
        Elem::Bit => v::from_bits(&bytes, t.dims.map(|d| d as usize))
            .into_iter()
            .map(f64::from)
            .collect(),
        _ => v::from_i8(&bytes).into_iter().map(f64::from).collect(),
    })
}

/// A `_source` value of a vector field as its result cell: the canonical
/// JSON array text for dense vectors (see [`numbers`]); anything else, and
/// sparse vectors, as [`J::cell`] has it.
pub fn cell(val: &J, t: &VecType) -> serde_json::Value {
    if t.sparse {
        return val.cell();
    }
    match numbers(val, t) {
        Some(n) if t.single && t.elem == Elem::Float => v::json(n.into_iter().map(|x| x as f32)),
        Some(n) => v::json(n),
        None => val.cell(),
    }
}

/// The canonical text ([`v::text`]) of a JSON array of numbers given as
/// text, written number by number without holding them (a transfer's
/// vector costs about its text): `None` when it isn't one.
pub fn canonical_text(raw: &str) -> Option<String> {
    canonical_numbers(raw, false)
}

/// [`canonical_text`], each number as the float32 it is when `single`.
fn canonical_numbers(raw: &str, single: bool) -> Option<String> {
    use std::fmt::Write;
    let inner = raw.trim().strip_prefix('[')?.strip_suffix(']')?;
    let mut out = String::with_capacity(inner.len() + 2);
    out.push('[');
    if !inner.trim().is_empty() {
        for (i, tok) in inner.split(',').enumerate() {
            let t = tok.trim();
            // `inf`, `NaN` and `+1` parse in Rust but aren't JSON numbers.
            if t.is_empty()
                || t.starts_with('+')
                || t.chars()
                    .any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E')
            {
                return None;
            }
            let f: f64 = t.parse().ok().filter(|f: &f64| f.is_finite())?;
            if i > 0 {
                out.push(',');
            }
            if single {
                let _ = write!(out, "{}", f as f32);
            } else {
                let _ = write!(out, "{f}");
            }
        }
    }
    out.push(']');
    Some(out)
}

/// A `_source` value's raw text as a vector cell's text: dense float and
/// byte arrays straight from the text ([`canonical_text`]), the rest
/// (bits, hex strings) through [`numbers`].
pub fn raw_text(raw: &str, t: &VecType) -> Option<String> {
    if t.sparse {
        return None;
    }
    if t.elem != Elem::Bit && raw.trim_start().starts_with('[') {
        return canonical_numbers(raw, t.single && t.elem == Elem::Float);
    }
    numbers(&J::parse(raw).ok()?, t).map(v::text)
}

/// A vector cell to load (JSON array text, or a hex string) as the JSON
/// the field takes: bit vectors given one 0 / 1 per dimension are packed
/// into their bytes. `None` when the text isn't a vector.
pub fn load_json(text: &str, t: &VecType) -> Option<String> {
    if t.elem != Elem::Bit {
        return canonical_text(text);
    }
    let values = v::parse(text)?;
    if t.elem == Elem::Bit {
        let packed = t.dims.is_some_and(|d| d as usize == values.len())
            || (t.dims.is_none() && values.iter().all(|&x| x == 0.0 || x == 1.0));
        if packed {
            let bytes = v::to_bits(&values);
            return Some(v::text(bytes.iter().map(|&b| b as i8)));
        }
    }
    Some(v::text(values))
}

/// The vector fields of a mapping (`{"properties": …}`), by their dotted
/// path, nested objects included: one vector per value (a multi-vector
/// `rank_vectors` keeps its JSON).
pub fn mapping_vectors(mapping: &J) -> Vec<(String, VecType)> {
    fn walk(props: Option<&J>, prefix: &str, out: &mut Vec<(String, VecType)>) {
        for (name, f) in props.and_then(J::as_obj).into_iter().flatten() {
            let full = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            if let Some(t) = VecType::from_mapping(f).filter(|t| !t.multi) {
                out.push((full.clone(), t));
            }
            walk(f.get("properties"), &full, out);
        }
    }
    let mut out = Vec::new();
    walk(mapping.get("properties"), "", &mut out);
    out
}

// -- metrics -----------------------------------------------------------------------------------

/// A native distance as its standard name (or the engine's own when no
/// standard one fits).
fn metric_from(flavor: Flavor, native: &str, t: &VecType) -> String {
    let n = native.trim().to_ascii_lowercase();
    let std = match flavor {
        Flavor::Elastic => match n.as_str() {
            "cosine" => metric::COSINE,
            // Elasticsearch measures bit vectors' l2_norm as Hamming.
            "l2_norm" if t.elem == Elem::Bit => metric::HAMMING,
            "l2_norm" => metric::L2,
            "dot_product" | "max_inner_product" => metric::IP,
            _ => return n,
        },
        Flavor::OpenSearch | Flavor::OpenDistro => match n.as_str() {
            "l2" => metric::L2,
            "cosinesimil" => metric::COSINE,
            "innerproduct" => metric::IP,
            "l1" => metric::L1,
            "hamming" | "hammingbit" => metric::HAMMING,
            _ => return n,
        },
    };
    std.to_string()
}

/// The engine's name for a distance, standard or native.
fn metric_to(flavor: Flavor, m: &str, t: &VecType) -> Option<&'static str> {
    let m = m.trim().to_ascii_lowercase();
    Some(match flavor {
        Flavor::Elastic => match m.as_str() {
            "cosine" => "cosine",
            "l2" | "l2_norm" => "l2_norm",
            "hamming" if t.elem == Elem::Bit => "l2_norm",
            "ip" | "dot_product" => "dot_product",
            "max_inner_product" => "max_inner_product",
            _ => return None,
        },
        Flavor::OpenSearch => match m.as_str() {
            "l2" => "l2",
            "cosine" | "cosinesimil" => "cosinesimil",
            "ip" | "innerproduct" => "innerproduct",
            "l1" => "l1",
            "linf" => "linf",
            "hamming" => "hamming",
            "hammingbit" => "hammingbit",
            _ => return None,
        },
        // Open Distro's k-NN 1.13 has these three.
        Flavor::OpenDistro => match m.as_str() {
            "l2" => "l2",
            "cosine" | "cosinesimil" => "cosinesimil",
            "l1" => "l1",
            _ => return None,
        },
    })
}

/// The native distance to write: the index's own spelling when it still
/// means the chosen metric (`max_inner_product` and `dot_product` are both
/// `ip`), else the chosen one's.
fn native_metric(
    flavor: Flavor,
    ix: &IndexDef,
    vi: &VectorIndex,
    t: &VecType,
    key: &str,
) -> Result<Option<String>> {
    let Some(m) = vi
        .metric
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    else {
        return Ok(None);
    };
    // A standard name keeps the index's own spelling of it; an engine's
    // name is what it says.
    let standard = metric::ALL.iter().any(|s| s.eq_ignore_ascii_case(m));
    if let Some(own) = ix.options.get(key).filter(|_| standard) {
        if metric_from(flavor, own, t) == metric_from(flavor, m, t)
            && metric_to(flavor, own, t).is_some()
        {
            return Ok(Some(own.clone()));
        }
    }
    match metric_to(flavor, m, t) {
        Some(n) => Ok(Some(n.to_string())),
        None => Err(Error::Query(format!(
            "El índice vectorial «{}»: {} no tiene la distancia «{m}».",
            ix.name,
            flavor.name()
        ))),
    }
}

// -- reading the indexes -----------------------------------------------------------------------

/// A parameter's JSON value as text: scalars as they are, the rest compact.
fn param_text(v: &J) -> String {
    v.text()
}

/// A parameter's text as the JSON the mapping takes: numbers, booleans,
/// objects and arrays as such, the rest as a string.
pub fn param_json(s: &str) -> J {
    let t = s.trim();
    match t {
        "true" => return J::Bool(true),
        "false" => return J::Bool(false),
        _ => {}
    }
    if let Ok(n) = serde_json::from_str::<serde_json::Number>(t) {
        return J::Num(n);
    }
    if t.starts_with('{') || t.starts_with('[') {
        if let Ok(j) = J::parse(t) {
            return j;
        }
    }
    J::Str(t.to_string())
}

fn is_true(v: Option<&J>) -> bool {
    matches!(v, Some(J::Bool(true))) || v.and_then(J::as_str) == Some("true")
}

/// Elasticsearch's vector index of a `dense_vector` field (`index: true`).
fn es_index(path: &str, f: &J, t: &VecType) -> Option<IndexDef> {
    if !is_true(f.get("index")) {
        return None;
    }
    let mut vi = VectorIndex::default();
    let mut options = BTreeMap::new();
    if let Some(s) = f.get("similarity").map(J::text).filter(|s| !s.is_empty()) {
        vi.metric = Some(metric_from(Flavor::Elastic, &s, t));
        options.insert("similarity".to_string(), s);
    }
    if let Some(io) = f.get("index_options").and_then(J::as_obj) {
        for (k, val) in io {
            if k == "type" {
                let ty = val.text();
                let (quant, alg) = match ty.rsplit_once('_') {
                    Some((q, a)) => (Some(q.to_string()), a.to_string()),
                    // An unquantized float index says so: left empty, the
                    // engine would quantize it (`int8_hnsw` since 8.14).
                    None if quantizable(t) => (Some(NO_QUANTIZATION.to_string()), ty.clone()),
                    None => (None, ty.clone()),
                };
                vi.algorithm = Some(alg);
                if let Some(q) = quant {
                    vi.params.insert(param::QUANTIZATION.into(), q);
                }
            } else {
                vi.params.insert(k.clone(), param_text(val));
            }
        }
        options.insert(
            "index_options".to_string(),
            J::Obj(sorted(io.clone())).compact(),
        );
    }
    Some(IndexDef {
        name: path.to_string(),
        columns: vec![path.to_string()],
        options,
        vector: Some(vi),
        ..Default::default()
    })
}

/// The quantization that says "none" (an index of the plain type).
const NO_QUANTIZATION: &str = "none";

/// Elasticsearch quantizes float vectors' indexes (bytes and bits aren't).
fn quantizable(t: &VecType) -> bool {
    matches!(t.elem, Elem::Float | Elem::Bf16)
}

/// Elasticsearch's distance when a vector index doesn't say: `cosine`, and
/// `l2_norm` (Hamming) for bits, the only one they have. Written always,
/// since 8.0–8.10 require it with `index: true`.
fn es_default_similarity(t: &VecType) -> &'static str {
    if t.elem == Elem::Bit {
        "l2_norm"
    } else {
        "cosine"
    }
}

fn sorted(mut o: Obj) -> Obj {
    o.sort_by(|a, b| a.0.cmp(&b.0));
    o
}

/// OpenSearch's vector index of a `knn_vector` field, when the index has
/// `index.knn`.
fn os_index(path: &str, f: &J, t: &VecType) -> IndexDef {
    let mut vi = VectorIndex::default();
    let mut options = BTreeMap::new();
    if let Some(id) = f.get("model_id") {
        vi.params.insert("model_id".into(), param_text(id));
        options.insert("model_id".into(), param_text(id));
    }
    let method = f.get("method");
    if let Some(m) = method {
        if let Some(n) = m.get("name").map(J::text).filter(|n| !n.is_empty()) {
            vi.algorithm = Some(n.to_ascii_lowercase());
        }
        if let Some(e) = m.get("engine").map(J::text).filter(|e| !e.is_empty()) {
            vi.params.insert("engine".into(), e);
        }
        for (k, val) in m
            .get("parameters")
            .and_then(J::as_obj)
            .into_iter()
            .flatten()
        {
            vi.params.insert(k.clone(), param_text(val));
        }
        options.insert("method".into(), canonical(m).compact());
    }
    let space = method
        .and_then(|m| m.get("space_type"))
        .or_else(|| f.get("space_type"))
        .map(J::text)
        .filter(|s| !s.is_empty());
    if let Some(s) = space {
        vi.metric = Some(metric_from(Flavor::OpenSearch, &s, t));
        options.insert("space_type".into(), s);
    }
    for k in ["mode", "compression_level"] {
        if let Some(val) = f.get(k) {
            vi.params.insert(k.into(), param_text(val));
            options.insert(k.into(), param_text(val));
        }
    }
    IndexDef {
        name: path.to_string(),
        columns: vec![path.to_string()],
        options,
        vector: Some(vi),
        ..Default::default()
    }
}

/// A JSON value with every object's keys sorted, to compare.
fn canonical(j: &J) -> J {
    match j {
        J::Obj(o) => J::Obj(sorted(
            o.iter().map(|(k, v)| (k.clone(), canonical(v))).collect(),
        )),
        J::Arr(a) => J::Arr(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// Open Distro's k-NN settings: the index's `index.knn.space_type` and
/// `index.knn.algo_param.m` / `ef_construction` (flat or nested).
pub(crate) const OD_SETTINGS: &[(&str, &str)] = &[
    ("knn.space_type", ""),
    ("knn.algo_param.m", param::M),
    ("knn.algo_param.ef_construction", param::EF_CONSTRUCTION),
];

fn od_setting(settings: Option<&J>, key: &str) -> Option<String> {
    let s = settings?;
    let parts: Vec<&str> = key.split('.').collect();
    let mut nested = vec!["index"];
    nested.extend(&parts);
    s.get(&format!("index.{key}"))
        .or_else(|| s.at(&nested))
        .or_else(|| s.get(key))
        .map(J::text)
        .filter(|v| !v.is_empty())
}

/// Open Distro's vector index of a `knn_vector` field: the index's k-NN
/// settings (HNSW, `l2` unless the index says otherwise).
fn od_index(path: &str, settings: Option<&J>, t: &VecType) -> IndexDef {
    let mut vi = VectorIndex {
        algorithm: Some("hnsw".into()),
        ..Default::default()
    };
    let mut options = BTreeMap::new();
    let space = od_setting(settings, "knn.space_type");
    vi.metric = Some(metric_from(
        Flavor::OpenDistro,
        space.as_deref().unwrap_or("l2"),
        t,
    ));
    if let Some(s) = space {
        options.insert("knn.space_type".into(), s);
    }
    for (key, std) in &OD_SETTINGS[1..] {
        if let Some(val) = od_setting(settings, key) {
            vi.params.insert(std.to_string(), val);
        }
    }
    IndexDef {
        name: path.to_string(),
        columns: vec![path.to_string()],
        options,
        vector: Some(vi),
        ..Default::default()
    }
}

/// The vector index a field's mapping has, if any: `knn` is the index's
/// `index.knn` (OpenSearch, Open Distro), `settings` its settings.
pub(crate) fn read_index(
    flavor: Flavor,
    path: &str,
    f: &J,
    t: &VecType,
    knn: bool,
    settings: Option<&J>,
) -> Option<IndexDef> {
    if t.sparse || t.multi {
        return None;
    }
    match (flavor, t.base.as_str()) {
        (_, "dense_vector") => es_index(path, f, t),
        (Flavor::OpenDistro, "knn_vector") if knn => Some(od_index(path, settings, t)),
        (_, "knn_vector") if knn => Some(os_index(path, f, t)),
        _ => None,
    }
}

/// The mapping keys a field's vector index (read by [`read_index`]) takes
/// from it: they don't go in the column's options.
pub(crate) fn index_keys(flavor: Flavor, t: &VecType, indexed: bool) -> &'static [&'static str] {
    match (flavor, t.base.as_str(), indexed) {
        (_, "dense_vector", true) => &["index", "similarity", "index_options"],
        // Not indexed: no vector index says so.
        (_, "dense_vector", false) => &["index"],
        (Flavor::OpenDistro, _, _) => &[],
        (_, "knn_vector", true) => &[
            "method",
            "space_type",
            "mode",
            "compression_level",
            "model_id",
        ],
        _ => &[],
    }
}

// -- writing the indexes -----------------------------------------------------------------------

fn put(o: &mut Obj, key: &str, v: J) {
    match o.iter_mut().find(|(k, _)| k == key) {
        Some(e) => e.1 = v,
        None => o.push((key.to_string(), v)),
    }
}

/// A field's mapping (`m`, with its type and dimension) with its vector
/// index `ix` written in: Elasticsearch's `index`, `similarity` and
/// `index_options`; OpenSearch's `method`, `mode`, `compression_level` or
/// `model_id`. Open Distro's go in the index settings ([`od_settings`]).
pub(crate) fn write_index(flavor: Flavor, m: &mut Obj, ix: &IndexDef, t: &VecType) -> Result<()> {
    let vi = ix.vector.clone().unwrap_or_default();
    let alg = vi
        .algorithm
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_ascii_lowercase);
    let mut params: Vec<(String, String)> = vi
        .params
        .iter()
        .map(|(k, v)| (k.clone(), v.trim().to_string()))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    match flavor {
        Flavor::Elastic => {
            put(m, "index", J::Bool(true));
            let similarity = native_metric(flavor, ix, &vi, t, "similarity")?
                .unwrap_or_else(|| es_default_similarity(t).to_string());
            put(m, "similarity", J::Str(similarity));
            let quant = params
                .iter()
                .position(|(k, _)| k == param::QUANTIZATION)
                .map(|i| params.remove(i).1.to_ascii_lowercase());
            let alg = alg.unwrap_or_else(|| "hnsw".into());
            // Nothing chosen: the engine's index (`int8_hnsw` for floats
            // since 8.14, `bbq_hnsw` from 384 dimensions since 9.1).
            if quant.is_none() && params.is_empty() && alg == "hnsw" {
                return Ok(());
            }
            let ty = match quant.as_deref() {
                Some(NO_QUANTIZATION) => alg,
                Some(q) => format!("{q}_{alg}"),
                // HNSW parameters without a quantization: the engine's
                // default one for floats (int8), none for bytes and bits.
                None if alg == "hnsw" && quantizable(t) => format!("int8_{alg}"),
                None => alg,
            };
            let mut io: Obj = vec![("type".into(), J::Str(ty))];
            for (k, v) in params {
                io.push((k, param_json(&v)));
            }
            put(m, "index_options", J::Obj(io));
        }
        Flavor::OpenSearch => {
            if let Some(id) = params.iter().find(|(k, _)| k == "model_id") {
                put(m, "model_id", J::Str(id.1.clone()));
                return Ok(());
            }
            let mut space = native_metric(flavor, ix, &vi, t, "space_type")?;
            let mut top: Vec<(String, J)> = Vec::new();
            let mut engine = None;
            let mut parameters: Obj = Vec::new();
            for (k, v) in params {
                match k.as_str() {
                    "mode" | "compression_level" => top.push((k, J::Str(v))),
                    "engine" => engine = Some(v.to_ascii_lowercase()),
                    _ => parameters.push((k, param_json(&v))),
                }
            }
            if engine.is_none() {
                engine = os_engine(ix, t, &mut space)?.map(str::to_string);
            }
            if alg.is_some() || space.is_some() || engine.is_some() || !parameters.is_empty() {
                let mut method: Obj =
                    vec![("name".into(), J::Str(alg.unwrap_or_else(|| "hnsw".into())))];
                if let Some(s) = space {
                    method.push(("space_type".into(), J::Str(s)));
                }
                if let Some(e) = engine {
                    method.push(("engine".into(), J::Str(e)));
                }
                if !parameters.is_empty() {
                    method.push(("parameters".into(), J::Obj(parameters)));
                }
                put(m, "method", J::Obj(method));
            }
            for (k, v) in top {
                put(m, &k, v);
            }
        }
        Flavor::OpenDistro => {}
    }
    Ok(())
}

/// The engine an OpenSearch vector index that names none gets, when the
/// server's default may not take it: the default was NMSLIB up to 2.17
/// (floats only) and Faiss from 2.18 (no L1 / L∞). Bits go to Faiss with
/// Hamming (the only engine and distance for them), bytes to Lucene
/// (unless `mode` / `compression_level` pick the engine), and L1 / L∞ to
/// NMSLIB, the only one with them. Other floats keep the server's default
/// (on 2.18 alone, Faiss without `cosinesimil`: there cosine needs the
/// engine chosen). `space` is the native distance; bits get `hamming` when
/// it's empty.
fn os_engine(
    ix: &IndexDef,
    t: &VecType,
    space: &mut Option<String>,
) -> Result<Option<&'static str>> {
    let vi = ix.vector.as_ref();
    let by_mode = vi.is_some_and(|v| {
        ["mode", "compression_level"]
            .iter()
            .any(|k| v.params.get(*k).is_some_and(|x| !x.trim().is_empty()))
    });
    Ok(match t.elem {
        Elem::Bit => {
            match space.as_deref() {
                None => *space = Some("hamming".into()),
                Some("hamming" | "hammingbit") => {}
                Some(other) => {
                    return Err(Error::Query(format!(
                        "El índice vectorial «{}»: OpenSearch busca los vectores binary solo con la distancia hamming, no «{other}».",
                        ix.name.trim()
                    )))
                }
            }
            Some("faiss")
        }
        Elem::Byte if !by_mode => Some("lucene"),
        Elem::Float => match space.as_deref() {
            Some("l1" | "linf") => Some("nmslib"),
            _ => None,
        },
        _ => None,
    })
}

/// Open Distro's index settings for its vector indexes: they all share
/// the index's distance and HNSW parameters, so they have to agree.
pub(crate) fn od_settings(ixs: &[(&IndexDef, &VecType)]) -> Result<Obj> {
    let mut out: Obj = Vec::new();
    let mut first: Option<(String, Vec<(String, String)>)> = None;
    for (ix, t) in ixs {
        let vi = ix.vector.clone().unwrap_or_default();
        if let Some(a) = vi
            .algorithm
            .as_deref()
            .filter(|a| !a.trim().is_empty() && !a.trim().eq_ignore_ascii_case("hnsw"))
        {
            return Err(Error::Query(format!(
                "El índice vectorial «{}»: Open Distro solo tiene HNSW, no «{a}».",
                ix.name
            )));
        }
        let space = native_metric(Flavor::OpenDistro, ix, &vi, t, "knn.space_type")?;
        let mut params = Vec::new();
        for (k, val) in &vi.params {
            let key = match k.as_str() {
                param::M => "knn.algo_param.m",
                param::EF_CONSTRUCTION => "knn.algo_param.ef_construction",
                other => {
                    return Err(Error::Query(format!(
                        "El índice vectorial «{}»: Open Distro no tiene el parámetro «{other}» (solo m y ef_construction).",
                        ix.name
                    )))
                }
            };
            if !val.trim().is_empty() {
                params.push((key.to_string(), val.trim().to_string()));
            }
        }
        let space_key = space.clone().unwrap_or_default();
        match &first {
            None => {
                if let Some(s) = &space {
                    out.push(("knn.space_type".into(), J::Str(s.clone())));
                }
                for (k, v) in &params {
                    out.push((k.clone(), param_json(v)));
                }
                first = Some((space_key, params));
            }
            Some((s, p)) if *s == space_key && *p == params => {}
            Some(_) => {
                return Err(Error::Query(
                    "En Open Distro la distancia y los parámetros k-NN son del índice: todos sus índices vectoriales tienen que usar los mismos.".into(),
                ))
            }
        }
    }
    Ok(out)
}

// -- comparing them ----------------------------------------------------------------------------

/// Settings whose default the engine picks by version or by the vectors
/// (Elasticsearch's quantization and confidence interval, OpenSearch's
/// engine, mode and compression): left empty on one side, they match the
/// other side's.
const ENGINE_CHOICE: &[&str] = &[
    param::QUANTIZATION,
    "confidence_interval",
    "engine",
    "mode",
    "compression_level",
];

/// What a field's vector index searches with, with the engine's defaults
/// filled in; `None`: the field has no ANN index. `knn` is the index's
/// `index.knn` (OpenSearch indexes every `knn_vector` of a k-NN index).
fn effective(
    flavor: Flavor,
    ix: Option<&IndexDef>,
    t: &VecType,
    knn: bool,
) -> Option<BTreeMap<String, String>> {
    let implicit;
    let ix = match ix {
        Some(ix) => ix,
        None if flavor != Flavor::Elastic && knn && t.base == "knn_vector" => {
            implicit = IndexDef {
                vector: Some(VectorIndex::default()),
                ..Default::default()
            };
            &implicit
        }
        None => return None,
    };
    let vi = ix.vector.clone().unwrap_or_default();
    let lower = |s: &str| s.trim().to_ascii_lowercase();
    let mut out: BTreeMap<String, String> = vi
        .params
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, v)| (k.clone(), lower(v)))
        .collect();
    if out.contains_key("model_id") {
        return Some(out);
    }
    let alg = vi
        .algorithm
        .as_deref()
        .map(lower)
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| "hnsw".into());
    let key = match flavor {
        Flavor::Elastic => "similarity",
        Flavor::OpenSearch => "space_type",
        Flavor::OpenDistro => "knn.space_type",
    };
    let mut metric =
        native_metric(flavor, ix, &vi, t, key).unwrap_or_else(|_| vi.metric.as_deref().map(lower));
    match flavor {
        Flavor::Elastic => {
            metric.get_or_insert_with(|| es_default_similarity(t).to_string());
        }
        Flavor::OpenSearch => {
            if !out.contains_key("engine") {
                if let Ok(Some(e)) = os_engine(ix, t, &mut metric) {
                    out.insert("engine".into(), e.into());
                }
            }
            metric.get_or_insert_with(|| "l2".into());
        }
        Flavor::OpenDistro => {
            metric.get_or_insert_with(|| "l2".into());
        }
    }
    if alg == "hnsw" {
        let ef = if flavor == Flavor::OpenDistro {
            "512"
        } else {
            "100"
        };
        out.entry(param::M.into()).or_insert_with(|| "16".into());
        out.entry(param::EF_CONSTRUCTION.into())
            .or_insert_with(|| ef.into());
    }
    out.insert("algorithm".into(), alg);
    out.insert("metric".into(), metric.unwrap_or_default());
    Some(out)
}

/// Whether two vector indexes of field type `t` (`None`: none) search the
/// same way once the engine's defaults are filled in: what the designer
/// leaves empty matches what the server reads back (see [`effective`]).
pub(crate) fn same_index(
    flavor: Flavor,
    a: Option<&IndexDef>,
    b: Option<&IndexDef>,
    t: &VecType,
    knn: bool,
) -> bool {
    match (effective(flavor, a, t, knn), effective(flavor, b, t, knn)) {
        (None, None) => true,
        (Some(a), Some(b)) => a
            .keys()
            .chain(b.keys())
            .all(|k| match (a.get(k), b.get(k)) {
                (Some(x), Some(y)) => x == y,
                _ => ENGINE_CHOICE.contains(&k.as_str()),
            }),
        _ => false,
    }
}

// -- bit vectors written one bit per dimension --------------------------------------------------

/// The value at a field's dotted path in a document (`a.b` as one key or
/// as `b` inside `a`).
fn field_mut<'a>(doc: &'a mut J, path: &str) -> Option<&'a mut J> {
    let J::Obj(o) = doc else {
        return None;
    };
    if let Some(i) = o.iter().position(|(k, _)| k == path) {
        return Some(&mut o[i].1);
    }
    let split = path
        .match_indices('.')
        .map(|(i, _)| i)
        .find(|&i| o.iter().any(|(k, _)| k == &path[..i]))?;
    let i = o.iter().position(|(k, _)| k == &path[..split])?;
    field_mut(&mut o[i].1, &path[split + 1..])
}

/// A bit vector of `dims` dimensions given one 0 or 1 per dimension, as
/// the signed bytes its field takes. `None` when it isn't one (a value
/// already packed is `dims / 8` long).
fn packed_bits(bits: &[f64], dims: Option<u32>) -> Option<Vec<i64>> {
    let dims = dims? as usize;
    if bits.len() != dims || dims < 8 || !bits.iter().all(|b| *b == 0.0 || *b == 1.0) {
        return None;
    }
    Some(
        v::to_bits(bits)
            .into_iter()
            .map(|b| i64::from(b as i8))
            .collect(),
    )
}

/// A document's bit (binary) vectors given one 0 or 1 per dimension, as
/// results show them, packed into the signed bytes the field takes (a
/// value already packed is `dims / 8` long and stays). `fields` are the
/// index's vector fields. Whether anything changed.
pub fn pack_bits(doc: &mut J, fields: &[(String, VecType)]) -> bool {
    let mut changed = false;
    for (path, t) in fields
        .iter()
        .filter(|(_, t)| t.elem == Elem::Bit && !t.multi)
    {
        let Some(val) = field_mut(doc, path) else {
            continue;
        };
        let J::Arr(a) = val else { continue };
        let bits: Option<Vec<f64>> = a
            .iter()
            .map(|x| match x {
                J::Num(n) => n.as_f64(),
                _ => None,
            })
            .collect();
        if let Some(bytes) = bits.and_then(|b| packed_bits(&b, t.dims)) {
            *val = J::Arr(bytes.into_iter().map(|b| J::Num(b.into())).collect());
            changed = true;
        }
    }
    changed
}

// -- scripts that know their columns' types ---------------------------------------------------

/// The vector type of a script's column, from its type (DBine's spelling,
/// `dense_vector(16, bit)`, as a result or a describe names it) or its
/// [`VectorMeta`]. `None`: not a dense vector.
pub fn script_type(c: &dbine_driver::ResultColumn) -> Option<VecType> {
    let meta = c.vector.as_ref();
    let mut t = match VecType::parse(&c.type_name) {
        Some(Ok(t)) => t,
        _ => {
            let m = meta?;
            let elem = match m.element {
                VectorElement::Bit => Elem::Bit,
                VectorElement::I8 => Elem::Byte,
                VectorElement::Bf16 => Elem::Bf16,
                _ => Elem::Float,
            };
            VecType::new("dense_vector", m.dims, elem)
        }
    };
    if t.dims.is_none() {
        t.dims = meta.and_then(|m| m.dims);
    }
    (!t.sparse && !t.multi && !meta.is_some_and(|m| m.sparse)).then_some(t)
}

/// A vector value of an insert or update script (a JSON array of numbers,
/// or its text) as its field takes it: a bit vector given one 0 or 1 per
/// dimension (as results show it) packed into its signed bytes, a byte
/// vector's numbers as integers. `None`: written as it is.
pub fn script_value(val: &serde_json::Value, t: &VecType) -> Option<serde_json::Value> {
    let nums = v::parse_value(val)?;
    let ints = |n: Vec<i64>| serde_json::Value::Array(n.into_iter().map(Into::into).collect());
    match t.elem {
        Elem::Bit => packed_bits(&nums, t.dims).map(ints),
        Elem::Byte => nums
            .iter()
            .map(|&x| (x.fract() == 0.0 && (-128.0..=127.0).contains(&x)).then_some(x as i64))
            .collect::<Option<Vec<i64>>>()
            .map(ints),
        Elem::Float | Elem::Bf16 => None,
    }
}

/// Script values written for their columns' vector types (see
/// [`script_value`]); `types` by position.
pub fn script_row(row: &[serde_json::Value], types: &[Option<VecType>]) -> Vec<serde_json::Value> {
    row.iter()
        .enumerate()
        .map(|(i, val)| {
            types
                .get(i)
                .and_then(Option::as_ref)
                .and_then(|t| script_value(val, t))
                .unwrap_or_else(|| val.clone())
        })
        .collect()
}

/// The indices a `_bulk` body writes documents to (`default`: the path's
/// index, if any).
pub fn bulk_indices(body: &str, default: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut lines = body.lines().filter(|l| !l.trim().is_empty());
    while let Some(line) = lines.next() {
        let Some((op, index)) = bulk_action(line, default) else {
            continue;
        };
        if op != "delete" {
            lines.next();
        }
        if let Some(i) = index.filter(|_| op != "delete") {
            if !out.contains(&i) {
                out.push(i);
            }
        }
    }
    out
}

/// A `_bulk` action line's operation and target index.
fn bulk_action(line: &str, default: Option<&str>) -> Option<(String, Option<String>)> {
    let a = J::parse(line).ok()?;
    let (op, meta) = a.as_obj()?.first()?;
    let index = meta
        .get("_index")
        .and_then(J::as_str)
        .or(default)
        .map(str::to_string);
    Some((op.clone(), index))
}

/// Vector fields by their dotted path.
pub type Fields = Vec<(String, VecType)>;

/// A `_bulk` body with its documents' bit vectors packed (see
/// [`pack_bits`]); `fields` gives each index's vector fields. `None`: no
/// change.
pub fn pack_bulk(
    body: &str,
    default: Option<&str>,
    fields: &dyn Fn(&str) -> Option<Fields>,
) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    let mut changed = false;
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        out.push(line.to_string());
        if line.trim().is_empty() {
            continue;
        }
        let Some((op, index)) = bulk_action(line, default) else {
            continue;
        };
        if op == "delete" {
            continue;
        }
        let Some(src) = lines.next() else { break };
        let packed = index.as_deref().and_then(fields).and_then(|f| {
            let mut doc = J::parse(src).ok()?;
            let hit = if op == "update" {
                pack_partial(&mut doc, &f)
            } else {
                pack_bits(&mut doc, &f)
            };
            hit.then(|| doc.compact())
        });
        changed |= packed.is_some();
        out.push(packed.unwrap_or_else(|| src.to_string()));
    }
    changed.then(|| out.join("\n"))
}

/// An update's body (`doc`, `upsert`, a data stream update's
/// `script.params.doc`) with its bit vectors packed.
pub fn pack_partial(body: &mut J, fields: &[(String, VecType)]) -> bool {
    let J::Obj(o) = body else {
        return false;
    };
    let mut changed = false;
    for (k, v) in o.iter_mut() {
        match k.as_str() {
            "doc" | "upsert" => changed |= pack_bits(v, fields),
            "script" => {
                if let Some(d) = field_mut(v, "params.doc") {
                    changed |= pack_bits(d, fields);
                }
            }
            _ => {}
        }
    }
    changed
}

/// A mapping body for Elasticsearch 7, whose `dense_vector` has no `index`
/// parameter (its vectors are never indexed): the `"index": false` DBine
/// writes for a vector field without a vector index taken out. `None`: no
/// change.
pub fn strip_unindexed(body: &str) -> Option<String> {
    fn walk(j: &mut J) -> bool {
        match j {
            J::Obj(o) => {
                let mut changed = false;
                if o.iter()
                    .any(|(k, v)| k == "type" && v.as_str() == Some("dense_vector"))
                {
                    let before = o.len();
                    o.retain(|(k, v)| !(k == "index" && *v == J::Bool(false)));
                    changed = o.len() != before;
                }
                for (_, v) in o.iter_mut() {
                    changed |= walk(v);
                }
                changed
            }
            J::Arr(a) => a.iter_mut().fold(false, |c, v| walk(v) | c),
            _ => false,
        }
    }
    let mut j = J::parse(body).ok()?;
    walk(&mut j).then(|| j.pretty())
}

/// The `dense_vector` fields a mapping body asks to index (`index: true`),
/// by name: what Elasticsearch 7 takes without effect (it has no ANN
/// index), so the user is told.
pub fn indexed_vectors(body: &str) -> Vec<String> {
    fn walk(j: &J, out: &mut Vec<String>) {
        match j {
            J::Obj(o) => {
                for (k, v) in o {
                    let indexed = v.get("type").and_then(J::as_str) == Some("dense_vector")
                        && is_true(v.get("index"));
                    if indexed && !out.contains(k) {
                        out.push(k.clone());
                    }
                    walk(v, out);
                }
            }
            J::Arr(a) => a.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(j) = J::parse(body) {
        walk(&j, &mut out);
    }
    out
}

// -- what the designer offers ------------------------------------------------------------------

/// The vector indexes the designer creates on `flavor`.
pub(crate) fn designer_spec(flavor: Flavor) -> VectorIndexSpec {
    let p =
        |key: &'static str, algorithms: &[&'static str], default: &'static str| VectorParamSpec {
            key,
            algorithms: algorithms.to_vec(),
            default,
        };
    match flavor {
        Flavor::Elastic => VectorIndexSpec {
            algorithms: vec!["hnsw", "flat"],
            metrics: vec![
                metric::COSINE,
                metric::L2,
                metric::IP,
                "max_inner_product",
                metric::HAMMING,
            ],
            params: vec![
                p(param::M, &["hnsw"], "16"),
                p(param::EF_CONSTRUCTION, &["hnsw"], "100"),
                // Empty: the engine's (int8 for floats since 8.14, bbq from
                // 384 dimensions since 9.1); `none` for a plain index.
                p(param::QUANTIZATION, &[], ""),
                p("confidence_interval", &[], ""),
            ],
            filter_columns: false,
        },
        Flavor::OpenSearch => VectorIndexSpec {
            algorithms: vec!["hnsw"],
            metrics: vec![
                metric::L2,
                metric::COSINE,
                metric::IP,
                metric::L1,
                "linf",
                metric::HAMMING,
            ],
            params: vec![
                p("engine", &[], ""),
                p(param::M, &[], "16"),
                p(param::EF_CONSTRUCTION, &[], "100"),
                p("mode", &[], ""),
                p("compression_level", &[], ""),
            ],
            filter_columns: false,
        },
        Flavor::OpenDistro => VectorIndexSpec {
            algorithms: vec!["hnsw"],
            metrics: vec![metric::L2, metric::COSINE, metric::L1],
            params: vec![
                p(param::M, &[], "16"),
                p(param::EF_CONSTRUCTION, &[], "512"),
            ],
            filter_columns: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn j(s: &str) -> J {
        J::parse(s).unwrap()
    }

    #[test]
    fn script_types_and_values() {
        use dbine_driver::ResultColumn;
        let t = |c: ResultColumn| script_type(&c).map(|t| (t.dims, t.elem));
        assert_eq!(
            t(ResultColumn::new("b", "dense_vector(16, bit)")),
            Some((Some(16), Elem::Bit))
        );
        assert_eq!(
            t(ResultColumn::new("b", "knn_vector(8, binary)")),
            Some((Some(8), Elem::Bit))
        );
        // A bare spelling takes its dimension from the meta.
        assert_eq!(
            t(ResultColumn::new("b", "dense_vector")
                .with_vector(VectorMeta::dense(Some(3), VectorElement::F32))),
            Some((Some(3), Elem::Float))
        );
        assert_eq!(
            t(ResultColumn::new("b", "vector(3)")
                .with_vector(VectorMeta::dense(Some(3), VectorElement::I8))),
            Some((Some(3), Elem::Byte))
        );
        // Not dense vectors.
        assert_eq!(t(ResultColumn::new("r", "rank_vectors(3)")), None);
        assert_eq!(
            t(ResultColumn::new("s", "sparse_vector")
                .with_vector(VectorMeta::sparse(None, VectorElement::F32))),
            None
        );
        assert_eq!(t(ResultColumn::new("k", "keyword")), None);

        let bits = VecType::new("dense_vector", Some(8), Elem::Bit);
        assert_eq!(
            script_value(&json!([1, 0, 0, 0, 0, 0, 0, 1]), &bits),
            Some(json!([-127]))
        );
        assert_eq!(
            script_value(&json!("[0,0,0,0,0,0,0,1]"), &bits),
            Some(json!([1]))
        );
        // Not bits of that length: as it is.
        assert_eq!(script_value(&json!([1, 2, 0, 0, 0, 0, 0, 1]), &bits), None);
        assert_eq!(script_value(&json!([5]), &bits), None);
        assert_eq!(script_value(&json!(1), &bits), None, "not a vector at all");
        let bytes = VecType::new("dense_vector", Some(2), Elem::Byte);
        assert_eq!(
            script_value(&json!([1.0, -128.0]), &bytes),
            Some(json!([1, -128]))
        );
        assert_eq!(script_value(&json!([1.0, 128.0]), &bytes), None);
        let floats = VecType::new("dense_vector", Some(2), Elem::Float);
        assert_eq!(script_value(&json!([1.0, 2.0]), &floats), None);
    }

    #[test]
    fn spellings_round_trip() {
        for (s, base, dims, elem) in [
            ("dense_vector(3)", "dense_vector", Some(3), Elem::Float),
            ("dense_vector(4, byte)", "dense_vector", Some(4), Elem::Byte),
            ("dense_vector(16, bit)", "dense_vector", Some(16), Elem::Bit),
            (
                "dense_vector(8, bfloat16)",
                "dense_vector",
                Some(8),
                Elem::Bf16,
            ),
            ("dense_vector", "dense_vector", None, Elem::Float),
            ("knn_vector(768)", "knn_vector", Some(768), Elem::Float),
            ("knn_vector(16, binary)", "knn_vector", Some(16), Elem::Bit),
        ] {
            let t = VecType::parse(s).unwrap().unwrap();
            assert_eq!((t.base.as_str(), t.dims, t.elem), (base, dims, elem), "{s}");
            assert_eq!(t.spelling(), s);
            assert_eq!(t.type_name, s);
        }
        // Other spellings of the same.
        let t = VecType::parse("KNN_VECTOR( 3 , BYTE )").unwrap().unwrap();
        assert_eq!(t.spelling(), "knn_vector(3, byte)");
        assert_eq!(
            VecType::parse("dense_vector(4, float)")
                .unwrap()
                .unwrap()
                .spelling(),
            "dense_vector(4)"
        );
        for bad in [
            "dense_vector(x)",
            "dense_vector(0)",
            "dense_vector(3, int)",
            "knn_vector(3, byte, 2)",
        ] {
            assert!(VecType::parse(bad).unwrap().is_err(), "{bad}");
        }
        assert!(VecType::parse("keyword").is_none());
        assert!(VecType::parse("sparse_vector").is_none());
    }

    #[test]
    fn mapping_types_and_meta() {
        let t = VecType::from_mapping(&j(
            r#"{"type":"dense_vector","element_type":"byte","dims":4,"index":true}"#,
        ))
        .unwrap();
        assert_eq!(t.spelling(), "dense_vector(4, byte)");
        assert_eq!(t.meta(), VectorMeta::dense(Some(4), VectorElement::I8));
        let t = VecType::from_mapping(&j(
            r#"{"type":"knn_vector","dimension":16,"data_type":"BINARY"}"#,
        ))
        .unwrap();
        assert_eq!(t.spelling(), "knn_vector(16, binary)");
        assert_eq!(t.meta(), VectorMeta::dense(Some(16), VectorElement::Bit));
        let t = VecType::from_mapping(&j(r#"{"type":"sparse_vector"}"#)).unwrap();
        assert!(t.sparse);
        assert_eq!(t.meta(), VectorMeta::sparse(None, VectorElement::F32));
        assert!(VecType::from_mapping(&j(r#"{"type":"keyword"}"#)).is_none());
        let found = mapping_vectors(&j(
            r#"{"properties":{"v":{"type":"dense_vector","dims":3},"doc":{"properties":{"e":{"type":"knn_vector","dimension":2}}},"t":{"type":"text"}}}"#,
        ));
        let names: Vec<String> = found
            .iter()
            .map(|(n, t)| format!("{n}:{}", t.spelling()))
            .collect();
        assert_eq!(names, ["v:dense_vector(3)", "doc.e:knn_vector(2)"]);
    }

    #[test]
    fn cells_are_canonical_arrays() {
        let float = VecType::new("dense_vector", Some(3), Elem::Float);
        assert_eq!(
            cell(&j("[0.1,0.2,0.30000001]"), &float),
            json!("[0.1,0.2,0.30000001]")
        );
        assert_eq!(cell(&j("[1.0,2.5,-3e-2]"), &float), json!("[1,2.5,-0.03]"));
        // Not a vector: as it is.
        assert_eq!(cell(&j(r#""x""#), &float), json!("x"));
        assert_eq!(cell(&j("[1,\"a\"]"), &float), json!("[1,\"a\"]"));
        let byte = VecType::new("dense_vector", Some(4), Elem::Byte);
        assert_eq!(cell(&j("[1,-2,3,127]"), &byte), json!("[1,-2,3,127]"));
        assert_eq!(cell(&j(r#""0102037f""#), &byte), json!("[1,2,3,127]"));
        assert_eq!(cell(&j(r#""ff80""#), &byte), json!("[-1,-128]"));
        let bit = VecType::new("dense_vector", Some(16), Elem::Bit);
        assert_eq!(
            cell(&j("[-127,64]"), &bit),
            json!("[1,0,0,0,0,0,0,1,0,1,0,0,0,0,0,0]")
        );
        assert_eq!(cell(&j(r#""8140""#), &bit), cell(&j("[-127,64]"), &bit));
        let sparse = VecType::new("sparse_vector", None, Elem::Float);
        assert_eq!(cell(&j(r#"{"a":1.5}"#), &sparse), json!("{\"a\":1.5}"));
    }

    #[test]
    fn single_precision_cells() {
        let mut t = VecType::new("knn_vector", Some(2), Elem::Float);
        t.single = true;
        // Java 17's text of a float: the same float32, shortest.
        assert_eq!(
            cell(&j("[3.6379788E-12,1.0]"), &t),
            json!("[0.000000000003637979,1]")
        );
        assert_eq!(
            raw_text("[3.6379788E-12, 1.0]", &t).as_deref(),
            Some("[0.000000000003637979,1]")
        );
    }

    #[test]
    fn loads_pack_bits() {
        let bit = VecType::new("dense_vector", Some(16), Elem::Bit);
        assert_eq!(
            load_json("[1,0,0,0,0,0,0,1,0,1,0,0,0,0,0,0]", &bit).as_deref(),
            Some("[-127,64]")
        );
        // Already packed: as it is.
        assert_eq!(load_json("[-127,64]", &bit).as_deref(), Some("[-127,64]"));
        let float = VecType::new("dense_vector", Some(3), Elem::Float);
        assert_eq!(
            load_json(" [1.0, 2, -3e-2] ", &float).as_deref(),
            Some("[1,2,-0.03]")
        );
        assert_eq!(load_json("abc", &float), None);
        assert_eq!(canonical_text("[]").as_deref(), Some("[]"));
        for bad in ["[1,,2]", "[inf]", "[+1]", "1,2", "[1,null]"] {
            assert_eq!(canonical_text(bad), None, "{bad}");
        }
        assert_eq!(raw_text("[1.50, 2]", &float).as_deref(), Some("[1.5,2]"));
        assert_eq!(
            raw_text("\"0a7f\"", &VecType::new("knn_vector", Some(2), Elem::Byte)).as_deref(),
            Some("[10,127]")
        );
    }

    fn es(f: &str) -> Option<IndexDef> {
        let f = j(f);
        let t = VecType::from_mapping(&f).unwrap();
        read_index(Flavor::Elastic, "v", &f, &t, false, None)
    }

    #[test]
    fn elasticsearch_indexes_read_and_write_back() {
        let ix = es(r#"{"type":"dense_vector","dims":3,"index":true,"similarity":"cosine","index_options":{"type":"int8_hnsw","m":16,"ef_construction":100}}"#).unwrap();
        let vi = ix.vector.clone().unwrap();
        assert_eq!(vi.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(vi.metric.as_deref(), Some("cosine"));
        assert_eq!(
            vi.params,
            BTreeMap::from([
                ("ef_construction".to_string(), "100".to_string()),
                ("m".to_string(), "16".to_string()),
                ("quantization".to_string(), "int8".to_string()),
            ])
        );
        assert_eq!(ix.columns, ["v"]);
        assert_eq!(
            ix.options.get("index_options").map(String::as_str),
            Some(r#"{"ef_construction":100,"m":16,"type":"int8_hnsw"}"#)
        );
        let t = VecType::new("dense_vector", Some(3), Elem::Float);
        let mut m: Obj = vec![("type".into(), J::Str("dense_vector".into()))];
        write_index(Flavor::Elastic, &mut m, &ix, &t).unwrap();
        assert_eq!(
            J::Obj(m).compact(),
            r#"{"type":"dense_vector","index":true,"similarity":"cosine","index_options":{"type":"int8_hnsw","ef_construction":100,"m":16}}"#
        );
        // Not indexed: no vector index.
        assert!(es(r#"{"type":"dense_vector","dims":3,"index":false}"#).is_none());
        // max_inner_product is `ip`, and stays itself on the way back.
        let ix = es(r#"{"type":"dense_vector","dims":3,"index":true,"similarity":"max_inner_product","index_options":{"type":"hnsw","m":8}}"#).unwrap();
        assert_eq!(ix.vector.as_ref().unwrap().metric.as_deref(), Some("ip"));
        let mut m = Vec::new();
        write_index(Flavor::Elastic, &mut m, &ix, &t).unwrap();
        assert_eq!(
            J::Obj(m).compact(),
            r#"{"index":true,"similarity":"max_inner_product","index_options":{"type":"hnsw","m":8}}"#
        );
        // Bits: l2_norm is Hamming.
        let ix = es(r#"{"type":"dense_vector","element_type":"bit","dims":16,"index":true,"similarity":"l2_norm"}"#).unwrap();
        assert_eq!(
            ix.vector.as_ref().unwrap().metric.as_deref(),
            Some("hamming")
        );
        assert_eq!(ix.vector.as_ref().unwrap().algorithm, None);
        // From the designer: standard names.
        let ix = IndexDef {
            name: "v_idx".into(),
            columns: vec!["v".into()],
            vector: Some(VectorIndex {
                algorithm: Some("flat".into()),
                metric: Some("ip".into()),
                params: BTreeMap::from([("quantization".into(), "int4".into())]),
            }),
            ..Default::default()
        };
        let mut m = Vec::new();
        write_index(Flavor::Elastic, &mut m, &ix, &t).unwrap();
        assert_eq!(
            J::Obj(m).compact(),
            r#"{"index":true,"similarity":"dot_product","index_options":{"type":"int4_flat"}}"#
        );
        // Only a metric: the engine's index options.
        let ix = IndexDef {
            name: "v".into(),
            columns: vec!["v".into()],
            vector: Some(VectorIndex {
                metric: Some("l2".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut m = Vec::new();
        write_index(Flavor::Elastic, &mut m, &ix, &t).unwrap();
        assert_eq!(
            J::Obj(m).compact(),
            r#"{"index":true,"similarity":"l2_norm"}"#
        );
        let mut bad = ix.clone();
        bad.vector.as_mut().unwrap().metric = Some("l1".into());
        let e = write_index(Flavor::Elastic, &mut Vec::new(), &bad, &t).unwrap_err();
        assert!(e.to_string().contains("«l1»"), "{e}");
    }

    #[test]
    fn opensearch_indexes_read_and_write_back() {
        let f = j(
            r#"{"type":"knn_vector","dimension":3,"method":{"engine":"lucene","space_type":"innerproduct","name":"hnsw","parameters":{"ef_construction":50,"m":8}}}"#,
        );
        let t = VecType::from_mapping(&f).unwrap();
        assert!(read_index(Flavor::OpenSearch, "v", &f, &t, false, None).is_none());
        let ix = read_index(Flavor::OpenSearch, "v", &f, &t, true, None).unwrap();
        let vi = ix.vector.clone().unwrap();
        assert_eq!(vi.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(vi.metric.as_deref(), Some("ip"));
        assert_eq!(
            vi.params,
            BTreeMap::from([
                ("ef_construction".to_string(), "50".to_string()),
                ("engine".to_string(), "lucene".to_string()),
                ("m".to_string(), "8".to_string()),
            ])
        );
        let mut m = Vec::new();
        write_index(Flavor::OpenSearch, &mut m, &ix, &t).unwrap();
        assert_eq!(
            J::Obj(m).compact(),
            r#"{"method":{"name":"hnsw","space_type":"innerproduct","engine":"lucene","parameters":{"ef_construction":50,"m":8}}}"#
        );
        // Top-level space type, mode and compression; an encoder as JSON.
        let f = j(
            r#"{"type":"knn_vector","dimension":8,"space_type":"cosinesimil","mode":"on_disk","compression_level":"16x"}"#,
        );
        let ix = read_index(Flavor::OpenSearch, "v", &f, &t, true, None).unwrap();
        let vi = ix.vector.clone().unwrap();
        assert_eq!((vi.algorithm, vi.metric.as_deref()), (None, Some("cosine")));
        let mut m = Vec::new();
        write_index(Flavor::OpenSearch, &mut m, &ix, &t).unwrap();
        assert_eq!(
            J::Obj(m).compact(),
            r#"{"method":{"name":"hnsw","space_type":"cosinesimil"},"compression_level":"16x","mode":"on_disk"}"#
        );
        let f = j(
            r#"{"type":"knn_vector","dimension":3,"method":{"engine":"faiss","space_type":"l2","name":"hnsw","parameters":{"encoder":{"name":"sq","parameters":{"type":"fp16"}}}}}"#,
        );
        let ix = read_index(Flavor::OpenSearch, "v", &f, &t, true, None).unwrap();
        let mut m = Vec::new();
        write_index(Flavor::OpenSearch, &mut m, &ix, &t).unwrap();
        assert_eq!(
            J::Obj(m).compact(),
            r#"{"method":{"name":"hnsw","space_type":"l2","engine":"faiss","parameters":{"encoder":{"name":"sq","parameters":{"type":"fp16"}}}}}"#
        );
        // A trained model.
        let f = j(r#"{"type":"knn_vector","model_id":"my-model"}"#);
        let ix = read_index(Flavor::OpenSearch, "v", &f, &t, true, None).unwrap();
        let mut m = Vec::new();
        write_index(Flavor::OpenSearch, &mut m, &ix, &t).unwrap();
        assert_eq!(J::Obj(m).compact(), r#"{"model_id":"my-model"}"#);
        // A vector index with nothing chosen: the engine's method.
        let ix = IndexDef {
            name: "x".into(),
            columns: vec!["v".into()],
            vector: Some(VectorIndex::default()),
            ..Default::default()
        };
        let mut m = Vec::new();
        write_index(Flavor::OpenSearch, &mut m, &ix, &t).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn elasticsearch_quantization_is_the_engines_unless_chosen() {
        let t = VecType::new("dense_vector", Some(3), Elem::Float);
        let write = |vi: VectorIndex, t: &VecType| {
            let ix = IndexDef {
                name: "v".into(),
                columns: vec!["v".into()],
                vector: Some(vi),
                ..Default::default()
            };
            let mut m = Vec::new();
            write_index(Flavor::Elastic, &mut m, &ix, t).unwrap();
            J::Obj(m).compact()
        };
        let p = |kv: &[(&str, &str)]| -> BTreeMap<String, String> {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        // The designer's defaults (HNSW, a distance, nothing else): the
        // engine's index, quantized as it does.
        assert_eq!(
            write(
                VectorIndex {
                    algorithm: Some("hnsw".into()),
                    metric: Some("cosine".into()),
                    ..Default::default()
                },
                &t
            ),
            r#"{"index":true,"similarity":"cosine"}"#
        );
        // Parameters without a quantization: int8, the engine's default.
        assert_eq!(
            write(
                VectorIndex {
                    algorithm: Some("hnsw".into()),
                    params: p(&[("m", "32")]),
                    ..Default::default()
                },
                &t
            ),
            r#"{"index":true,"similarity":"cosine","index_options":{"type":"int8_hnsw","m":32}}"#
        );
        // `none`: plain HNSW; bytes are never quantized; flat stays flat.
        assert_eq!(
            write(
                VectorIndex {
                    params: p(&[("quantization", "none"), ("m", "32")]),
                    ..Default::default()
                },
                &t
            ),
            r#"{"index":true,"similarity":"cosine","index_options":{"type":"hnsw","m":32}}"#
        );
        let bytes = VecType::new("dense_vector", Some(3), Elem::Byte);
        assert_eq!(
            write(
                VectorIndex {
                    params: p(&[("m", "8")]),
                    ..Default::default()
                },
                &bytes
            ),
            r#"{"index":true,"similarity":"cosine","index_options":{"type":"hnsw","m":8}}"#
        );
        assert_eq!(
            write(
                VectorIndex {
                    algorithm: Some("flat".into()),
                    ..Default::default()
                },
                &t
            ),
            r#"{"index":true,"similarity":"cosine","index_options":{"type":"flat"}}"#
        );
        // A plain float index reads back as `none`, so it's written plain.
        let ix = es(r#"{"type":"dense_vector","dims":3,"index":true,"similarity":"cosine","index_options":{"type":"hnsw","m":16,"ef_construction":100}}"#).unwrap();
        assert_eq!(
            ix.vector
                .as_ref()
                .unwrap()
                .params
                .get("quantization")
                .map(String::as_str),
            Some("none")
        );
        let mut m = Vec::new();
        write_index(Flavor::Elastic, &mut m, &ix, &t).unwrap();
        assert_eq!(
            J::Obj(m).compact(),
            r#"{"index":true,"similarity":"cosine","index_options":{"type":"hnsw","ef_construction":100,"m":16}}"#
        );
        // Bytes have no quantization to say.
        let ix = es(r#"{"type":"dense_vector","element_type":"byte","dims":3,"index":true,"similarity":"cosine","index_options":{"type":"hnsw"}}"#).unwrap();
        assert!(ix.vector.as_ref().unwrap().params.is_empty());
        let spec = designer_spec(Flavor::Elastic);
        let q = spec
            .params
            .iter()
            .find(|p| p.key == "quantization")
            .unwrap();
        assert_eq!(q.default, "");
    }

    #[test]
    fn opensearch_indexes_get_the_engine_their_vectors_need() {
        let write = |vi: VectorIndex, t: &VecType| {
            let ix = IndexDef {
                name: "v_idx".into(),
                columns: vec!["v".into()],
                vector: Some(vi),
                ..Default::default()
            };
            let mut m = Vec::new();
            write_index(Flavor::OpenSearch, &mut m, &ix, t).map(|_| J::Obj(m).compact())
        };
        let float = VecType::new("knn_vector", Some(3), Elem::Float);
        let byte = VecType::new("knn_vector", Some(3), Elem::Byte);
        let bits = VecType::new("knn_vector", Some(16), Elem::Bit);
        let hnsw = |m: Option<&str>| VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: m.map(Into::into),
            ..Default::default()
        };
        // Bytes: Lucene (NMSLIB, the default up to 2.17, refuses them).
        assert_eq!(
            write(hnsw(Some("cosine")), &byte).unwrap(),
            r#"{"method":{"name":"hnsw","space_type":"cosinesimil","engine":"lucene"}}"#
        );
        // Bits: Faiss with Hamming, the only ones; another distance is refused.
        assert_eq!(
            write(hnsw(None), &bits).unwrap(),
            r#"{"method":{"name":"hnsw","space_type":"hamming","engine":"faiss"}}"#
        );
        assert_eq!(
            write(hnsw(Some("hamming")), &bits).unwrap(),
            r#"{"method":{"name":"hnsw","space_type":"hamming","engine":"faiss"}}"#
        );
        let e = write(hnsw(Some("l2")), &bits).unwrap_err().to_string();
        assert!(e.contains("hamming") && e.contains("v_idx"), "{e}");
        // L1 / L∞: NMSLIB, the only one with them; the rest of the floats
        // keep the server's default engine.
        assert_eq!(
            write(hnsw(Some("l1")), &float).unwrap(),
            r#"{"method":{"name":"hnsw","space_type":"l1","engine":"nmslib"}}"#
        );
        assert_eq!(
            write(hnsw(Some("cosine")), &float).unwrap(),
            r#"{"method":{"name":"hnsw","space_type":"cosinesimil"}}"#
        );
        // An engine chosen is kept, and a mode picks it for bytes.
        let mut faiss = hnsw(Some("l2"));
        faiss.params.insert("engine".into(), "faiss".into());
        assert_eq!(
            write(faiss, &byte).unwrap(),
            r#"{"method":{"name":"hnsw","space_type":"l2","engine":"faiss"}}"#
        );
        let mut disk = hnsw(Some("l2"));
        disk.params.insert("mode".into(), "on_disk".into());
        assert_eq!(
            write(disk, &byte).unwrap(),
            r#"{"method":{"name":"hnsw","space_type":"l2"},"mode":"on_disk"}"#
        );
    }

    #[test]
    fn rank_vectors_are_typed_but_not_vector_columns() {
        let t = VecType::parse("rank_vectors(128, byte)").unwrap().unwrap();
        assert!(t.multi);
        assert_eq!(t.spelling(), "rank_vectors(128, byte)");
        let f = j(r#"{"type":"rank_vectors","dims":128}"#);
        let t = VecType::from_mapping(&f).unwrap();
        assert_eq!(t.spelling(), "rank_vectors(128)");
        assert!(read_index(Flavor::Elastic, "r", &f, &t, false, None).is_none());
        let found = mapping_vectors(&j(
            r#"{"properties":{"r":{"type":"rank_vectors","dims":2},"v":{"type":"dense_vector","dims":2}}}"#,
        ));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "v");
    }

    #[test]
    fn bit_vectors_written_one_bit_per_dimension_are_packed() {
        let fields = vec![
            (
                "bits".to_string(),
                VecType::new("dense_vector", Some(16), Elem::Bit),
            ),
            (
                "doc.b".to_string(),
                VecType::new("knn_vector", Some(8), Elem::Bit),
            ),
            (
                "f".to_string(),
                VecType::new("dense_vector", Some(16), Elem::Float),
            ),
        ];
        let unpacked = "[1,0,0,0,0,0,0,1,0,1,0,0,0,0,0,0]";
        let mut doc = j(&format!(
            r#"{{"bits":{unpacked},"f":{unpacked},"doc":{{"b":[0,0,0,0,0,0,1,1]}},"t":"x"}}"#
        ));
        assert!(pack_bits(&mut doc, &fields));
        assert_eq!(
            doc.compact(),
            format!(r#"{{"bits":[-127,64],"f":{unpacked},"doc":{{"b":[3]}},"t":"x"}}"#)
        );
        // Already packed (dims / 8 bytes), a hex string, the wrong length:
        // as they are.
        for same in [
            r#"{"bits":[-127,64]}"#,
            r#"{"bits":"8140"}"#,
            r#"{"bits":[1,0,1]}"#,
            r#"{"doc.b":[3]}"#,
        ] {
            let mut d = j(same);
            assert!(!pack_bits(&mut d, &fields), "{same}");
        }
        let mut d = j(r#"{"doc.b":[0,0,0,0,0,0,1,1]}"#);
        assert!(pack_bits(&mut d, &fields));
        assert_eq!(d.compact(), r#"{"doc.b":[3]}"#);

        // In a _bulk body, by each action's index; updates' doc and upsert.
        let body = format!(
            "{{\"index\":{{\"_index\":\"a\",\"_id\":\"1\"}}}}\n{{\"bits\":{unpacked}}}\n{{\"delete\":{{\"_id\":\"2\"}}}}\n{{\"update\":{{\"_id\":\"3\"}}}}\n{{\"doc\":{{\"bits\":{unpacked}}},\"upsert\":{{\"bits\":{unpacked}}}}}\n{{\"create\":{{\"_index\":\"other\"}}}}\n{{\"bits\":{unpacked}}}"
        );
        assert_eq!(bulk_indices(&body, Some("b")), ["a", "b", "other"]);
        let map = |i: &str| (i != "other").then(|| fields.clone());
        let packed = pack_bulk(&body, Some("b"), &map).unwrap();
        assert_eq!(
            packed.lines().collect::<Vec<_>>(),
            [
                r#"{"index":{"_index":"a","_id":"1"}}"#,
                r#"{"bits":[-127,64]}"#,
                r#"{"delete":{"_id":"2"}}"#,
                r#"{"update":{"_id":"3"}}"#,
                r#"{"doc":{"bits":[-127,64]},"upsert":{"bits":[-127,64]}}"#,
                r#"{"create":{"_index":"other"}}"#,
                &format!(r#"{{"bits":{unpacked}}}"#),
            ]
        );
        assert!(pack_bulk(r#"{"index":{"_index":"other"}}"#, None, &map).is_none());
        // A data stream's update (DBine's script copies params.doc).
        let mut u = j(&format!(
            r#"{{"query":{{"ids":{{"values":["1"]}}}},"script":{{"source":"x","params":{{"doc":{{"bits":{unpacked}}}}}}}}}"#
        ));
        assert!(pack_partial(&mut u, &fields));
        assert!(u.compact().contains(r#""doc":{"bits":[-127,64]}"#));
    }

    #[test]
    fn elasticsearch_7_takes_vectors_without_index_false() {
        let body = r#"{"mappings":{"properties":{"v":{"type":"dense_vector","dims":3,"index":false},"k":{"type":"keyword","index":false}}}}"#;
        let out = strip_unindexed(body).unwrap();
        let got: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            got,
            json!({"mappings": {"properties": {"v": {"type": "dense_vector", "dims": 3}, "k": {"type": "keyword", "index": false}}}})
        );
        assert!(
            strip_unindexed(r#"{"properties":{"v":{"type":"dense_vector","dims":3}}}"#).is_none()
        );
        // A vector index asked for stays (7.17 takes it without effect,
        // older 7.x refuse it): the fields are named in a warning.
        assert!(
            strip_unindexed(r#"{"properties":{"v":{"type":"dense_vector","index":true}}}"#)
                .is_none()
        );
        assert_eq!(
            indexed_vectors(
                r#"{"mappings":{"properties":{"v":{"type":"dense_vector","index":true},"w":{"type":"dense_vector","index":false},"o":{"properties":{"x":{"type":"dense_vector","dims":2,"index":true}}}}}}"#
            ),
            ["v", "x"]
        );
    }

    #[test]
    fn open_distro_indexes_are_the_index_settings() {
        let s = j(
            r#"{"index.knn":"true","index.knn.space_type":"cosinesimil","index.knn.algo_param.m":"8"}"#,
        );
        let f = j(r#"{"type":"knn_vector","dimension":3}"#);
        let t = VecType::from_mapping(&f).unwrap();
        let ix = read_index(Flavor::OpenDistro, "v", &f, &t, true, Some(&s)).unwrap();
        let vi = ix.vector.clone().unwrap();
        assert_eq!(vi.metric.as_deref(), Some("cosine"));
        assert_eq!(
            vi.params,
            BTreeMap::from([("m".to_string(), "8".to_string())])
        );
        let out = od_settings(&[(&ix, &t), (&ix, &t)]).unwrap();
        assert_eq!(
            J::Obj(out).compact(),
            r#"{"knn.space_type":"cosinesimil","knn.algo_param.m":8}"#
        );
        // Defaults: l2.
        let ix = read_index(Flavor::OpenDistro, "v", &f, &t, true, None).unwrap();
        assert_eq!(ix.vector.as_ref().unwrap().metric.as_deref(), Some("l2"));
        let mut other = ix.clone();
        other.vector.as_mut().unwrap().metric = Some("cosine".into());
        assert!(od_settings(&[(&ix, &t), (&other, &t)]).is_err());
        other.vector.as_mut().unwrap().metric = Some("ip".into());
        assert!(od_settings(&[(&other, &t)]).is_err());
    }
}
