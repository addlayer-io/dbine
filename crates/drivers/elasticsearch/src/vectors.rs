//! Vectors (embeddings) in Elasticsearch, OpenSearch and Open Distro, and
//! the pieces of them the Solr driver shares.
//!
//! - **Types.** Elasticsearch `dense_vector` (`dims`, `element_type` float,
//!   byte, bit or bfloat16) and `sparse_vector`; OpenSearch and Open Distro
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
//!   A vector field without a vector index gets the engine's default
//!   (Elasticsearch 8.11+ indexes it; OpenSearch only with `index.knn`).
//!   The native settings also go in [`IndexDef::options`] so a schema
//!   compare sees them.
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
        if ty != "dense_vector" && ty != "knn_vector" {
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
        if !matches!(base.as_str(), "dense_vector" | "knn_vector") {
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
/// path, nested objects included.
pub fn mapping_vectors(mapping: &J) -> Vec<(String, VecType)> {
    fn walk(props: Option<&J>, prefix: &str, out: &mut Vec<(String, VecType)>) {
        for (name, f) in props.and_then(J::as_obj).into_iter().flatten() {
            let full = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            if let Some(t) = VecType::from_mapping(f) {
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
    if t.sparse {
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
            if let Some(s) = native_metric(flavor, ix, &vi, t, "similarity")? {
                put(m, "similarity", J::Str(s));
            }
            if alg.is_none() && params.is_empty() {
                return Ok(());
            }
            let quant = params
                .iter()
                .position(|(k, _)| k == param::QUANTIZATION)
                .map(|i| params.remove(i).1)
                .filter(|q| !q.eq_ignore_ascii_case("none"));
            let alg = alg.unwrap_or_else(|| "hnsw".into());
            let ty = match quant {
                Some(q) => format!("{}_{alg}", q.to_ascii_lowercase()),
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
            let space = native_metric(flavor, ix, &vi, t, "space_type")?;
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
                p(param::QUANTIZATION, &[], "none"),
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
