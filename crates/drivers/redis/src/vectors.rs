//! Vectors (embeddings) in Redis and the servers that speak its protocol.
//!
//! - **Search indexes** (`FT.*`: RediSearch in Redis Stack and Redis 8,
//!   valkey-search, Dragonfly): a `VECTOR` field of an index over hashes is
//!   a packed blob of its `TYPE` (`FLOAT32`, `FLOAT64`, `FLOAT16`,
//!   `BFLOAT16`, `INT8`, `UINT8`), and of an index over JSON documents an
//!   array. [`FtIndex`] is one index as `FT.INFO` describes it (each server
//!   words it its own way), and gives back its `FT.CREATE`.
//! - **Vector sets** (Redis 8): a key of type `vectorset`, its elements with
//!   a vector each (`VEMB`) and JSON attributes (`VGETATTR`).
//!
//! DBine shows a vector as its canonical text (`[0.1,0.2,0.3]`, see
//! [`dbine_driver::vector`]): a blob is decoded by the `TYPE` of the index
//! that covers its key, and a vector edited in the grid is written back as
//! that blob, in a double-quoted argument with `\xHH` escapes.
//!
//! DBine spells a vector field's type `VECTOR(3, FLOAT32)` (dimension and
//! element type), in describes, the designer and transfers.

use crate::shape::text_of;
use dbine_driver::vector::{self, metric, param};
use dbine_driver::{VectorElement, VectorIndex, VectorMeta};
use redis::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The element type of a vector field (`TYPE` in `FT.CREATE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VType {
    #[default]
    F32,
    F64,
    F16,
    Bf16,
    I8,
    U8,
}

impl VType {
    pub const ALL: [VType; 6] = [
        VType::F32,
        VType::F64,
        VType::F16,
        VType::Bf16,
        VType::I8,
        VType::U8,
    ];

    /// `FLOAT32`, `float64`, `fp16`, `bfloat16`, `int8`, `uint8`…
    pub fn parse(s: &str) -> Option<VType> {
        Some(match s.trim().to_ascii_uppercase().as_str() {
            "FLOAT32" | "FP32" | "F32" => VType::F32,
            "FLOAT64" | "FP64" | "F64" => VType::F64,
            "FLOAT16" | "FP16" | "F16" => VType::F16,
            "BFLOAT16" | "BF16" => VType::Bf16,
            "INT8" | "I8" => VType::I8,
            "UINT8" | "U8" => VType::U8,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            VType::F32 => "FLOAT32",
            VType::F64 => "FLOAT64",
            VType::F16 => "FLOAT16",
            VType::Bf16 => "BFLOAT16",
            VType::I8 => "INT8",
            VType::U8 => "UINT8",
        }
    }

    pub fn element(self) -> VectorElement {
        match self {
            VType::F32 => VectorElement::F32,
            VType::F64 => VectorElement::F64,
            VType::F16 => VectorElement::F16,
            VType::Bf16 => VectorElement::Bf16,
            VType::I8 => VectorElement::I8,
            VType::U8 => VectorElement::U8,
        }
    }

    /// A blob of this type as the canonical vector text; `None` when its
    /// length isn't `dims` elements of it (`dims` `None`: any length).
    pub fn decode(self, b: &[u8], dims: Option<u32>) -> Option<String> {
        let size = match self {
            VType::F32 => 4,
            VType::F64 => 8,
            VType::F16 | VType::Bf16 => 2,
            VType::I8 | VType::U8 => 1,
        };
        if b.is_empty() || dims.is_some_and(|d| d as usize * size != b.len()) {
            return None;
        }
        Some(match self {
            VType::F32 => vector::text(vector::from_f32_le(b)?),
            VType::F64 => vector::text(vector::from_f64_le(b)?),
            VType::F16 => vector::text(vector::from_f16_le(b)?),
            VType::Bf16 => vector::text(vector::from_bf16_le(b)?),
            VType::I8 => vector::text(vector::from_i8(b)),
            VType::U8 => vector::text(vector::from_u8(b)),
        })
    }

    /// The blob of these numbers.
    pub fn encode(self, v: &[f64]) -> Vec<u8> {
        match self {
            VType::F32 => vector::to_f32_le(v),
            VType::F64 => vector::to_f64_le(v),
            VType::F16 => vector::to_f16_le(v),
            VType::Bf16 => vector::to_bf16_le(v),
            VType::I8 => vector::to_i8(v),
            VType::U8 => vector::to_u8(v),
        }
    }
}

/// `VECTOR(3, FLOAT32)`.
pub fn spell(dims: u32, t: VType) -> String {
    format!("VECTOR({dims}, {})", t.name())
}

/// A vector type as typed in the designer or read from another engine:
/// `VECTOR(3, FLOAT32)`, `vector(3)` (float32), `vector(3, int8)`,
/// `vector<float, 3>` (Cassandra), `VECTOR(FLOAT32, 3)` (Oracle)… The
/// dimension, when it says one, and the element type.
pub fn parse_spelling(s: &str) -> Option<(Option<u32>, VType)> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    // pgvector's halfvec and libSQL's F32_BLOB too.
    let (rest, mut ty) = if let Some(r) = lower.strip_prefix("halfvec") {
        (r, VType::F16)
    } else if let Some(r) = lower.strip_prefix("f32_blob") {
        (r, VType::F32)
    } else {
        (lower.strip_prefix("vector")?, VType::F32)
    };
    let inner = rest
        .trim()
        .strip_prefix(['(', '<'])
        .and_then(|r| r.strip_suffix([')', '>']))
        .unwrap_or("");
    let mut dims = None;
    for part in inner.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Ok(n) = part.parse::<u32>() {
            dims = Some(n);
        } else if let Some(v) = VType::parse(part) {
            ty = v;
        } else {
            match part {
                "float" | "real" | "float4" => ty = VType::F32,
                "double" | "float8" => ty = VType::F64,
                "tinyint" => ty = VType::I8,
                "*" => {}
                _ => return None,
            }
        }
    }
    if !rest.trim().is_empty() && inner.is_empty() && !rest.trim().starts_with(['(', '<']) {
        return None;
    }
    Some((dims, ty))
}

/// A vector value of a script or a cell (a JSON array of numbers, or text
/// that holds one) as the blob of `t`.
pub fn pack(v: &serde_json::Value, t: VType) -> Option<Vec<u8>> {
    vector::parse_value(v).map(|n| t.encode(&n))
}

/// Bytes as a double-quoted editor argument (`"\x00\x00\x80?"`): printable
/// ASCII as it is, everything else as `\xHH`.
pub fn quote_bytes(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 4 + 2);
    s.push('"');
    for &c in b {
        match c {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            0x20..=0x7e => s.push(c as char),
            _ => s.push_str(&format!("\\x{c:02x}")),
        }
    }
    s.push('"');
    s
}

/// One vector field of a search index.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FtVector {
    /// `HNSW`, `FLAT`, `SVS-VAMANA`.
    pub algorithm: String,
    pub ty: VType,
    pub dims: u32,
    /// `COSINE`, `L2`, `IP`.
    pub metric: String,
    /// Build and search settings, as `FT.CREATE` names them (`M`,
    /// `EF_CONSTRUCTION`, `EF_RUNTIME`, `COMPRESSION`…), in order.
    pub params: Vec<(String, String)>,
}

impl FtVector {
    pub fn meta(&self) -> VectorMeta {
        VectorMeta::dense(Some(self.dims), self.ty.element())
    }

    /// As [`dbine_driver::IndexDef::vector`] has it: lower-case algorithm,
    /// standard metric and parameter names.
    pub fn index(&self) -> VectorIndex {
        VectorIndex {
            algorithm: Some(self.algorithm.to_ascii_lowercase()),
            metric: Some(metric_std(&self.metric)),
            params: self
                .params
                .iter()
                .map(|(k, v)| (param_std(k), v.clone()))
                .collect(),
        }
    }

    /// From an [`IndexDef::vector`](dbine_driver::IndexDef::vector) and the
    /// field's type.
    pub fn from_index(v: &VectorIndex, ty: VType, dims: u32) -> Result<FtVector, String> {
        let algorithm = v
            .algorithm
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .unwrap_or("hnsw")
            .to_ascii_uppercase();
        if !matches!(algorithm.as_str(), "HNSW" | "FLAT" | "SVS-VAMANA") {
            return Err(format!(
                "Redis no tiene el algoritmo «{algorithm}»: HNSW, FLAT o SVS-VAMANA"
            ));
        }
        let metric = match v.metric.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            None => "COSINE".to_string(),
            Some(m) => metric_ft(m).ok_or_else(|| {
                format!("Redis no tiene la distancia «{m}»: coseno (COSINE), euclídea (L2) o producto interno (IP)")
            })?,
        };
        let mut params: Vec<(String, String)> = v
            .params
            .iter()
            .filter(|(_, val)| !val.trim().is_empty())
            .map(|(k, val)| (param_ft(k), val.trim().to_string()))
            .collect();
        // In the order FT.INFO lists them, so what is read and what is
        // made compare alike.
        let rank = |k: &str| {
            VECTOR_PARAMS
                .iter()
                .position(|p| p.eq_ignore_ascii_case(k))
                .unwrap_or(VECTOR_PARAMS.len())
        };
        params.sort_by_key(|(k, _)| rank(k));
        Ok(FtVector {
            algorithm,
            ty,
            dims,
            metric,
            params,
        })
    }
}

/// `COSINE` → `cosine`, `L2` → `l2`, `IP` → `ip`.
fn metric_std(m: &str) -> String {
    match m.to_ascii_uppercase().as_str() {
        "COSINE" => metric::COSINE.into(),
        "L2" => metric::L2.into(),
        "IP" => metric::IP.into(),
        other => other.to_ascii_lowercase(),
    }
}

/// A standard metric (or Redis' own) as `DISTANCE_METRIC`.
fn metric_ft(m: &str) -> Option<String> {
    let m = m.trim().to_ascii_lowercase();
    Some(
        match m.as_str() {
            "cosine" => "COSINE",
            "l2" | "euclidean" => "L2",
            "ip" | "dot" | "dot_product" | "inner_product" => "IP",
            _ => return None,
        }
        .into(),
    )
}

/// A build parameter's `FT.CREATE` name as the standard one, or the
/// engine's in lower case.
fn param_std(k: &str) -> String {
    match k.to_ascii_uppercase().as_str() {
        "M" => param::M.into(),
        "EF_CONSTRUCTION" => param::EF_CONSTRUCTION.into(),
        "COMPRESSION" => param::QUANTIZATION.into(),
        other => other.to_ascii_lowercase(),
    }
}

/// A standard (or engine) parameter name as `FT.CREATE` writes it.
fn param_ft(k: &str) -> String {
    match k.trim().to_ascii_lowercase().as_str() {
        "m" => "M".into(),
        "ef_construction" => "EF_CONSTRUCTION".into(),
        "quantization" => "COMPRESSION".into(),
        other => other.to_ascii_uppercase(),
    }
}

/// Vector settings `FT.CREATE` takes, as `FT.INFO` names them (lower
/// case); the rest of what `FT.INFO` says (sizes, memory) isn't one.
const VECTOR_PARAMS: &[&str] = &[
    "m",
    "ef_construction",
    "ef_runtime",
    "epsilon",
    "initial_cap",
    "block_size",
    "graph_max_degree",
    "construction_window_size",
    "search_window_size",
    "compression",
    "training_threshold",
    "reduce",
    "leanvec_dim",
];

/// One field of a search index.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FtField {
    /// The hash field or the JSONPath (`$.emb`).
    pub identifier: String,
    /// Its name in queries and results (`AS`).
    pub attribute: String,
    /// `TEXT`, `TAG`, `NUMERIC`, `GEO`, `GEOSHAPE`, `VECTOR`.
    pub kind: String,
    pub vector: Option<FtVector>,
    /// The rest of a non-vector field's `FT.CREATE` arguments (`WEIGHT 2`,
    /// `SEPARATOR ;`, `SORTABLE`…).
    pub args: Vec<String>,
}

impl FtField {
    /// The type as DBine shows it: `TEXT`, `TAG`… or `VECTOR(3, FLOAT32)`.
    pub fn data_type(&self) -> String {
        match &self.vector {
            Some(v) => spell(v.dims, v.ty),
            None => self.kind.clone(),
        }
    }

    fn is(&self, name: &str) -> bool {
        self.attribute == name || self.identifier == name
    }
}

/// A search index, as `FT.INFO` describes it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FtIndex {
    pub name: String,
    /// Over JSON documents (`ON JSON`), not hashes.
    pub on_json: bool,
    /// Key prefixes it covers (empty, or `""`: every key).
    pub prefixes: Vec<String>,
    pub filter: Option<String>,
    pub fields: Vec<FtField>,
}

/// Flags RediSearch lists bare among a field's attributes (RESP2).
const FLAGS: &[&str] = &[
    "SORTABLE",
    "UNF",
    "NOSTEM",
    "NOINDEX",
    "CASESENSITIVE",
    "WITHSUFFIXTRIE",
    "INDEXEMPTY",
    "INDEXMISSING",
];

fn items(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(a) | Value::Set(a) => a.iter().collect(),
        Value::Map(m) => m.iter().flat_map(|(k, v)| [k, v]).collect(),
        Value::Attribute { data, .. } => items(data),
        _ => Vec::new(),
    }
}

/// A reply's `key value` pairs (RESP2 flat array or RESP3 map), keys in
/// lower case; with `flags`, bare flag words are kept apart.
fn pairs(v: &Value, flags: bool) -> (Vec<(String, &Value)>, Vec<String>) {
    let mut out = Vec::new();
    let mut found = Vec::new();
    if let Value::Map(m) = v {
        for (k, val) in m {
            out.push((text_of(k).to_ascii_lowercase(), val));
        }
        return (out, found);
    }
    let list = items(v);
    let mut i = 0;
    while i < list.len() {
        let k = text_of(list[i]);
        if flags && FLAGS.contains(&k.to_ascii_uppercase().as_str()) {
            found.push(k.to_ascii_uppercase());
            i += 1;
            continue;
        }
        if let Some(val) = list.get(i + 1) {
            out.push((k.to_ascii_lowercase(), *val));
        }
        i += 2;
    }
    (out, found)
}

fn get<'a>(pairs: &'a [(String, &'a Value)], k: &str) -> Option<&'a Value> {
    pairs.iter().find(|(n, _)| n == k).map(|(_, v)| *v)
}

fn text(pairs: &[(String, &Value)], k: &str) -> Option<String> {
    get(pairs, k).map(text_of).filter(|s| !s.is_empty())
}

/// A number from `FT.INFO` as `FT.CREATE` takes it (`"0.01"`, `16`).
fn number(v: &Value) -> String {
    match v {
        Value::Double(f) => f.to_string(),
        other => text_of(other),
    }
}

impl FtIndex {
    /// `FT.INFO <name>`'s reply, in RediSearch's words (Redis Stack, Redis
    /// 8, Dragonfly) or valkey-search's. `None` when it isn't one.
    pub fn from_info(name: &str, reply: &Value) -> Option<FtIndex> {
        let (top, _) = pairs(reply, false);
        let def = get(&top, "index_definition")?;
        let (def, _) = pairs(def, false);
        let on_json = text(&def, "key_type").is_some_and(|k| k.eq_ignore_ascii_case("json"));
        let prefixes = get(&def, "prefixes")
            .map(|p| match p {
                Value::Array(_) | Value::Set(_) => items(p).into_iter().map(text_of).collect(),
                other => vec![text_of(other)],
            })
            .unwrap_or_default();
        let filter = text(&def, "filter");
        let mut fields = Vec::new();
        for a in get(&top, "attributes").map(items).unwrap_or_default() {
            if let Some(f) = field(a) {
                fields.push(f);
            }
        }
        Some(FtIndex {
            name: text(&top, "index_name").unwrap_or_else(|| name.to_string()),
            on_json,
            prefixes,
            filter,
            fields,
        })
    }

    /// Whether the index covers `key` (by its prefixes).
    pub fn covers(&self, key: &[u8]) -> bool {
        self.prefixes.is_empty() || self.prefixes.iter().any(|p| key.starts_with(p.as_bytes()))
    }

    /// The vector field named `name` (hash field or attribute).
    pub fn vector_field(&self, name: &str) -> Option<&FtVector> {
        self.fields
            .iter()
            .find(|f| f.is(name))
            .and_then(|f| f.vector.as_ref())
    }

    /// `FT.CREATE`'s arguments (the command name first).
    pub fn create_args(&self) -> Vec<String> {
        let mut a = vec![
            "FT.CREATE".to_string(),
            self.name.clone(),
            "ON".into(),
            if self.on_json { "JSON" } else { "HASH" }.into(),
        ];
        let prefixes: Vec<&String> = self.prefixes.iter().filter(|p| !p.is_empty()).collect();
        if !prefixes.is_empty() {
            a.push("PREFIX".into());
            a.push(prefixes.len().to_string());
            a.extend(prefixes.into_iter().cloned());
        }
        if let Some(f) = &self.filter {
            a.push("FILTER".into());
            a.push(f.clone());
        }
        a.push("SCHEMA".into());
        for f in &self.fields {
            a.push(f.identifier.clone());
            if f.attribute != f.identifier && !f.attribute.is_empty() {
                a.push("AS".into());
                a.push(f.attribute.clone());
            }
            match &f.vector {
                Some(v) => {
                    a.push("VECTOR".into());
                    a.push(v.algorithm.clone());
                    let mut p = vec![
                        "TYPE".to_string(),
                        v.ty.name().into(),
                        "DIM".into(),
                        v.dims.to_string(),
                        "DISTANCE_METRIC".into(),
                        v.metric.clone(),
                    ];
                    for (k, val) in &v.params {
                        p.push(k.clone());
                        p.push(val.clone());
                    }
                    a.push(p.len().to_string());
                    a.extend(p);
                }
                None => {
                    a.push(f.kind.clone());
                    a.extend(f.args.iter().cloned());
                }
            }
        }
        a
    }

    /// The `FT.CREATE` line, as the editor runs it.
    pub fn create_command(&self) -> String {
        crate::ddl::line_of(&self.create_args())
    }
}

/// One attribute of `FT.INFO`.
fn field(a: &Value) -> Option<FtField> {
    // valkey-search: every attribute is a pair (its flags are 0/1 values)
    // and `index` holds a vector's settings; RediSearch lists its flags
    // bare (RESP2) or in `flags` (RESP3).
    let valkey = items(a)
        .iter()
        .any(|x| text_of(x).eq_ignore_ascii_case("user_indexed_memory"));
    let (p, mut flags) = pairs(a, !valkey);
    let identifier = text(&p, "identifier")?;
    let attribute = text(&p, "attribute").unwrap_or_else(|| identifier.clone());
    let kind = text(&p, "type")?.to_ascii_uppercase();
    if let Some(Value::Array(list)) = get(&p, "flags") {
        flags.extend(list.iter().map(|f| text_of(f).to_ascii_uppercase()));
    }
    let on = |k: &str| get(&p, k).is_some_and(|v| matches!(text_of(v).as_str(), "1" | "true"));
    if valkey {
        for (k, flag) in [
            ("casesensitive", "CASESENSITIVE"),
            ("no_stem", "NOSTEM"),
            ("with_suffix_trie", "WITHSUFFIXTRIE"),
        ] {
            if on(k) {
                flags.push(flag.into());
            }
        }
    }
    let mut f = FtField {
        identifier,
        attribute,
        kind: kind.clone(),
        ..Default::default()
    };
    if kind == "VECTOR" {
        f.vector = Some(if let Some(ix) = get(&p, "index") {
            valkey_vector(ix)?
        } else {
            redisearch_vector(&p)?
        });
        return Some(f);
    }
    // A non-vector field's arguments, as FT.CREATE takes them (Dragonfly
    // writes the weight `1.000000`).
    if let Some(w) = get(&p, "weight")
        .map(number)
        .filter(|w| w.parse::<f64>().map_or(!w.is_empty(), |x| x != 1.0))
    {
        f.args.extend(["WEIGHT".into(), w]);
    }
    if let Some(s) = text(&p, "separator").filter(|s| s != ",") {
        f.args.extend(["SEPARATOR".into(), s]);
    }
    if let Some(ph) = text(&p, "phonetic") {
        f.args.extend(["PHONETIC".into(), ph]);
    }
    for flag in FLAGS {
        if flags.iter().any(|x| x == flag) {
            f.args.push(flag.to_string());
        }
    }
    Some(f)
}

/// RediSearch's vector attribute: `algorithm`, `data_type`, `dim`,
/// `distance_metric` and its settings, side by side.
fn redisearch_vector(p: &[(String, &Value)]) -> Option<FtVector> {
    let ty = VType::parse(&text(p, "data_type")?)?;
    let dims = text(p, "dim")?.parse().ok()?;
    let params = VECTOR_PARAMS
        .iter()
        .filter_map(|k| {
            let v = number(get(p, k)?);
            (!v.is_empty()).then(|| (k.to_ascii_uppercase(), compression(k, v)))
        })
        .collect();
    Some(FtVector {
        algorithm: text(p, "algorithm")?.to_ascii_uppercase(),
        ty,
        dims,
        metric: text(p, "distance_metric")?.to_ascii_uppercase(),
        params,
    })
}

/// SVS' compression as `FT.CREATE` takes it: where the server can't use
/// LVQ (no Intel CPU) it reports the scalar quantization it fell back to
/// (`GlobalSQ8`), which is what an `LVQ8` asks for there.
fn compression(k: &str, v: String) -> String {
    if k == "compression" && v.eq_ignore_ascii_case("GlobalSQ8") {
        "LVQ8".into()
    } else {
        v
    }
}

/// valkey-search's vector attribute: `index` → `dimensions`,
/// `distance_metric`, `data_type`, `capacity`, `algorithm` → `name` and its
/// settings.
fn valkey_vector(ix: &Value) -> Option<FtVector> {
    let (p, _) = pairs(ix, false);
    let ty = VType::parse(&text(&p, "data_type")?)?;
    let dims = text(&p, "dimensions")?.parse().ok()?;
    let (alg, _) = pairs(get(&p, "algorithm")?, false);
    let algorithm = text(&alg, "name")?.to_ascii_uppercase();
    let mut params: Vec<(String, String)> = VECTOR_PARAMS
        .iter()
        .filter_map(|k| {
            let v = number(get(&alg, k)?);
            (!v.is_empty()).then(|| (k.to_ascii_uppercase(), v))
        })
        .collect();
    if let Some(c) = get(&p, "capacity").map(number).filter(|c| !c.is_empty()) {
        params.push(("INITIAL_CAP".into(), c));
    }
    Some(FtVector {
        algorithm,
        ty,
        dims,
        metric: text(&p, "distance_metric")?.to_ascii_uppercase(),
        params,
    })
}

/// What the scripts (which run without a session) know of the server's
/// vectors: each search index seen (by name) and each vector set's
/// quantization (by key). Sessions fill it as they read them; it's only a
/// hint, the script says which type it wrote.
#[derive(Debug, Default)]
pub struct Known {
    pub indexes: HashMap<String, FtIndex>,
    /// Vector set → `VADD`'s quantization flag (`Q8`, `NOQUANT`, `BIN`).
    pub vsets: HashMap<String, String>,
}

pub type Shared = Arc<Mutex<Known>>;

impl Known {
    /// The vector field `field` of the hash `key`: its type and dimension,
    /// from an index over hashes that covers the key.
    pub fn hash_vector(&self, key: &str, field: &str) -> Option<&FtVector> {
        self.indexes
            .values()
            .filter(|i| !i.on_json && i.covers(key.as_bytes()))
            .find_map(|i| i.vector_field(field))
    }
}

/// `VINFO`'s `quant-type` as `VADD`'s flag.
pub fn quant_flag(quant_type: &str) -> Option<&'static str> {
    Some(match quant_type.trim().to_ascii_lowercase().as_str() {
        "int8" | "q8" => "Q8",
        "f32" | "noquant" | "float32" => "NOQUANT",
        "bin" | "binary" => "BIN",
        _ => return None,
    })
}

/// A vector of a vector set (`VEMB`'s reply: its numbers, dequantized) as
/// the canonical text, as 32-bit floats (what the set keeps).
pub fn vemb_text(v: &Value) -> Option<String> {
    let nums: Option<Vec<f32>> = items(v)
        .into_iter()
        .map(|x| match x {
            Value::Double(f) => Some(*f as f32),
            Value::Int(i) => Some(*i as f32),
            other => text_of(other).trim().parse::<f64>().ok().map(|f| f as f32),
        })
        .collect();
    nums.filter(|n| !n.is_empty()).map(vector::text)
}

/// `VADD key VALUES n v1 … vn element [flag]`'s arguments.
pub fn vadd_args(key: &str, element: &str, v: &[f64], flag: Option<&str>) -> Vec<String> {
    let mut a = vec![
        "VADD".to_string(),
        key.to_string(),
        "VALUES".into(),
        v.len().to_string(),
    ];
    a.extend(v.iter().map(|x| {
        // The shortest text that reads back the same 32-bit float.
        let t = vector::text([*x as f32]);
        t[1..t.len() - 1].to_string()
    }));
    a.push(element.to_string());
    if let Some(f) = flag {
        a.push(f.to_string());
    }
    a
}

/// A search index's fields as a table's columns, for describes: the
/// document's key first (`id`), then each field with its type.
pub fn index_columns(ix: &FtIndex) -> Vec<(String, String)> {
    std::iter::once(("id".to_string(), "key".to_string()))
        .chain(
            ix.fields
                .iter()
                .map(|f| (f.attribute.clone(), f.data_type())),
        )
        .collect()
}

/// Which server speaks the protocol: their search modules differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Server {
    /// RediSearch (Redis Stack, Redis 8): every type and algorithm, and
    /// vector sets.
    Redis,
    /// valkey-search: `FLOAT32` only, HNSW and FLAT; indexes in any
    /// database.
    Valkey,
    /// Dragonfly: every type, HNSW and FLAT; indexes in database 0.
    Dragonfly,
}

impl Server {
    pub fn of(driver_id: &str) -> Server {
        match driver_id {
            "valkey" => Server::Valkey,
            "dragonfly" => Server::Dragonfly,
            _ => Server::Redis,
        }
    }

    /// The vector types its indexes take.
    pub fn types(self) -> &'static [VType] {
        match self {
            Server::Valkey => &[VType::F32],
            _ => &VType::ALL,
        }
    }

    pub fn algorithms(self) -> Vec<&'static str> {
        match self {
            Server::Redis => vec!["hnsw", "flat", "svs-vamana"],
            _ => vec!["hnsw", "flat"],
        }
    }

    /// Search indexes cover only database 0 (RediSearch, Dragonfly), or
    /// each database its own (valkey-search).
    pub fn indexes_in(self, db: i64) -> bool {
        self == Server::Valkey || db == 0
    }
}

/// The build parameters the designer offers per algorithm, with their
/// defaults, as each server takes them.
pub fn designer_params(server: Server) -> Vec<dbine_driver::VectorParamSpec> {
    let p = |key: &'static str, algorithms: &[&'static str], default: &'static str| {
        dbine_driver::VectorParamSpec {
            key,
            algorithms: algorithms.to_vec(),
            default,
        }
    };
    match server {
        Server::Valkey => {
            return vec![
                p(param::M, &["hnsw"], "16"),
                p(param::EF_CONSTRUCTION, &["hnsw"], "200"),
                p("ef_runtime", &["hnsw"], "10"),
                p("initial_cap", &[], "10240"),
                p("block_size", &["flat"], "1024"),
            ]
        }
        Server::Dragonfly => {
            return vec![
                p(param::M, &["hnsw"], "16"),
                p(param::EF_CONSTRUCTION, &["hnsw"], "200"),
                p("ef_runtime", &["hnsw"], "10"),
                p("epsilon", &["hnsw"], "0.01"),
                p("initial_cap", &[], "1000"),
            ]
        }
        Server::Redis => {}
    }
    vec![
        p(param::M, &["hnsw"], "16"),
        p(param::EF_CONSTRUCTION, &["hnsw"], "200"),
        p("ef_runtime", &["hnsw"], "10"),
        p("epsilon", &["hnsw", "svs-vamana"], "0.01"),
        p("block_size", &["flat"], "1024"),
        p("graph_max_degree", &["svs-vamana"], "32"),
        p("construction_window_size", &["svs-vamana"], "200"),
        p("search_window_size", &["svs-vamana"], "10"),
        p(param::QUANTIZATION, &["svs-vamana"], ""),
        p("training_threshold", &["svs-vamana"], ""),
    ]
}

/// One document of a search result: its id, what was asked besides
/// (score, payload, sort key) and its fields.
type Doc = (
    serde_json::Value,
    Vec<serde_json::Value>,
    Vec<(String, serde_json::Value)>,
);

/// A search result's documents (`FT.SEARCH`'s reply) as rows: `id`, the
/// score and payload when asked for, then every field in order of
/// appearance. A vector field of `index` is decoded to its text and its
/// column marked; on a JSON index a returned vector is already an array.
pub fn search_table(
    args: &[Vec<u8>],
    reply: &Value,
    index: Option<&FtIndex>,
) -> crate::shape::Table {
    use serde_json::Value as J;
    let has = |w: &str| {
        args.iter()
            .skip(2)
            .any(|a| a.eq_ignore_ascii_case(w.as_bytes()))
    };
    let (scores, payloads, sortkeys, content) = (
        has("WITHSCORES"),
        has("WITHPAYLOADS"),
        has("WITHSORTKEYS"),
        !has("NOCONTENT"),
    );
    let list = items(reply);
    let mut t = crate::shape::Table::default();
    let Some(total) = list.first() else {
        return t;
    };
    let mut names: Vec<String> = Vec::new();
    // Each document: its id, the score / payload / sort key asked for, and
    // its fields.
    let mut docs: Vec<Doc> = Vec::new();
    let mut i = 1;
    while i < list.len() {
        let id = crate::shape::cell(list[i]);
        i += 1;
        let mut extra = Vec::new();
        for on in [scores, payloads, sortkeys] {
            if on && i < list.len() {
                extra.push(crate::shape::cell(list[i]));
                i += 1;
            }
        }
        let mut fields = Vec::new();
        if content && i < list.len() {
            let (kv, _) = pairs(list[i], false);
            // `pairs` lowers the keys: take the names from the reply.
            let raw = items(list[i]);
            for (n, (_, v)) in kv.iter().enumerate() {
                let name = raw.get(n * 2).map(|k| text_of(k)).unwrap_or_default();
                if !names.contains(&name) {
                    names.push(name.clone());
                }
                let cell = match (index.and_then(|ix| ix.vector_field(&name)), v) {
                    (Some(vf), Value::BulkString(b)) if !index.is_some_and(|ix| ix.on_json) => vf
                        .ty
                        .decode(b, Some(vf.dims))
                        .map(J::String)
                        .unwrap_or_else(|| crate::shape::cell(v)),
                    (Some(_), _) => {
                        let c = crate::shape::cell(v);
                        // JSON: `[1.5,2]` (or `[[1.5,2]]` with DIALECT 3).
                        match c.as_str().and_then(json_vector) {
                            Some(s) => J::String(s),
                            None => c,
                        }
                    }
                    (None, _) => crate::shape::cell(v),
                };
                fields.push((name, cell));
            }
            i += 1;
        }
        docs.push((id, extra, fields));
    }
    t.columns.push("id".into());
    if scores {
        t.columns.push("score".into());
    }
    if payloads {
        t.columns.push("payload".into());
    }
    if sortkeys {
        t.columns.push("sortkey".into());
    }
    let fixed = t.columns.len();
    t.columns.extend(names.iter().cloned());
    t.vectors = vec![None; t.columns.len()];
    t.types = vec![String::new(); t.columns.len()];
    if let Some(ix) = index {
        for (n, name) in names.iter().enumerate() {
            if let Some(v) = ix.vector_field(name) {
                t.vectors[fixed + n] = Some(v.meta());
                t.types[fixed + n] = spell(v.dims, v.ty);
            }
        }
    }
    for (id, extra, mut fields) in docs {
        let mut row = vec![id];
        row.extend(extra);
        for n in &names {
            let pos = fields.iter().position(|(k, _)| k == n);
            row.push(pos.map_or(J::Null, |p| fields.swap_remove(p).1));
        }
        t.rows.push(row);
    }
    t.message = Some(format!(
        "{}: {} documentos coinciden",
        String::from_utf8_lossy(&args[0]).to_ascii_uppercase(),
        text_of(total)
    ));
    t
}

/// A JSON vector as RediSearch returns it (`[1.5,2]`, or `[[1.5,2]]` with
/// DIALECT 3) as the canonical text.
fn json_vector(s: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let v = match v {
        serde_json::Value::Array(a) if a.len() == 1 && a[0].is_array() => a[0].clone(),
        v => v,
    };
    vector::parse_value(&v).map(vector::text)
}

/// `FT.AGGREGATE`'s rows (`[n, [f, v…], …]`, or with a cursor
/// `[[n, rows…], cursor]`): one row each, its fields as columns.
pub fn aggregate_table(reply: &Value) -> crate::shape::Table {
    let list = items(reply);
    let (rows, cursor) = match list.first() {
        Some(Value::Array(_)) => (items(list[0]), list.get(1).map(|c| text_of(c))),
        _ => (list, None),
    };
    let mut names: Vec<String> = Vec::new();
    let mut parsed = Vec::new();
    for r in rows.into_iter().skip(1) {
        let raw = items(r);
        let mut fields = Vec::new();
        for kv in raw.chunks(2) {
            let name = text_of(kv[0]);
            if !names.contains(&name) {
                names.push(name.clone());
            }
            fields.push((
                name,
                kv.get(1)
                    .map_or(serde_json::Value::Null, |v| crate::shape::cell(v)),
            ));
        }
        parsed.push(fields);
    }
    let mut t = crate::shape::Table {
        columns: names.clone(),
        ..Default::default()
    };
    for mut fields in parsed {
        t.rows.push(
            names
                .iter()
                .map(|n| {
                    fields
                        .iter()
                        .position(|(k, _)| k == n)
                        .map_or(serde_json::Value::Null, |p| fields.swap_remove(p).1)
                })
                .collect(),
        );
    }
    if let Some(c) = cursor {
        t.message = Some(format!("FT.AGGREGATE: cursor {c}"));
    }
    t
}

/// The command that shows a search index's first `limit` documents: all
/// of them (`*`), a JSON index's attributes returned (so its vectors come
/// as columns of their own). valkey-search has no `*`: a range over a
/// numeric field, or else the documents nearest a unit vector.
pub fn browse(name: &str, ix: Option<&FtIndex>, server: Server, limit: u32) -> String {
    use crate::command::quote_arg;
    let q = quote_arg(name);
    let Some(ix) = ix else {
        return format!("FT.SEARCH {q} * LIMIT 0 {limit}");
    };
    // A JSON index's attributes, each in a column (its whole document
    // otherwise comes in `$`).
    let ret = if ix.on_json && !ix.fields.is_empty() {
        let attrs: Vec<String> = ix.fields.iter().map(|f| quote_arg(&f.attribute)).collect();
        format!(" RETURN {} {}", attrs.len(), attrs.join(" "))
    } else {
        String::new()
    };
    if server == Server::Valkey {
        if let Some(f) = ix.fields.iter().find(|f| f.kind == "NUMERIC") {
            return format!(
                "FT.SEARCH {q} {}{ret} LIMIT 0 {limit}",
                quote_arg(&format!("@{}:[-inf +inf]", f.attribute))
            );
        }
        if let Some((f, v)) = ix
            .fields
            .iter()
            .find_map(|f| f.vector.as_ref().map(|v| (f, v)))
        {
            let mut unit = vec![0.0; v.dims as usize];
            if let Some(first) = unit.first_mut() {
                *first = 1.0;
            }
            return format!(
                "FT.SEARCH {q} {} PARAMS 2 q {}{ret} LIMIT 0 {limit} DIALECT 2",
                quote_arg(&format!("*=>[KNN {limit} @{} $q]", f.attribute)),
                quote_bytes(&v.ty.encode(&unit))
            );
        }
    }
    format!("FT.SEARCH {q} *{ret} LIMIT 0 {limit}")
}

/// `VSIM`'s reply (`element [score] [attributes]` each, as asked) as a
/// table.
pub fn vsim_table(args: &[Vec<u8>], reply: &Value) -> Option<crate::shape::Table> {
    use serde_json::Value as J;
    let has = |w: &str| {
        args.iter()
            .skip(2)
            .any(|a| a.eq_ignore_ascii_case(w.as_bytes()))
    };
    let (scores, attrs) = (has("WITHSCORES"), has("WITHATTRIBS"));
    let Value::Array(list) = reply else {
        return None;
    };
    let width = 1 + scores as usize + attrs as usize;
    let mut t = crate::shape::Table {
        columns: vec!["element".into()],
        ..Default::default()
    };
    if scores {
        t.columns.push("score".into());
    }
    if attrs {
        t.columns.push("attributes".into());
    }
    for chunk in list.chunks(width) {
        let mut row = vec![crate::shape::cell(&chunk[0])];
        let mut i = 1;
        if scores {
            let s = chunk.get(i).map(text_of).unwrap_or_default();
            row.push(
                s.parse::<f64>()
                    .map_or(J::String(s), dbine_driver::json_f64),
            );
            i += 1;
        }
        if attrs {
            row.push(chunk.get(i).map_or(J::Null, crate::shape::cell));
        }
        t.rows.push(row);
    }
    Some(t)
}

/// Commands whose reply holds no change to the indexes: the rest of
/// `FT.*` (`FT.CREATE`, `FT.ALTER`, `FT.DROPINDEX`…) makes the session
/// read them again.
pub(crate) const FT_READS: &[&str] = &[
    "FT.SEARCH",
    "FT.AGGREGATE",
    "FT.INFO",
    "FT._LIST",
    "FT.EXPLAIN",
    "FT.EXPLAINCLI",
    "FT.PROFILE",
    "FT.TAGVALS",
    "FT.SPELLCHECK",
    "FT.SYNDUMP",
    "FT.DICTDUMP",
    "FT.HYBRID",
    "FT.CONFIG",
    "FT.CURSOR",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }
    fn arr(v: Vec<Value>) -> Value {
        Value::Array(v)
    }

    #[test]
    fn blobs_round_trip_per_type() {
        let v = [0.5, -2.0, 3.25];
        for t in VType::ALL {
            let blob = t.encode(&v);
            let back = t.decode(&blob, Some(3)).unwrap();
            let want = match t {
                VType::I8 => "[1,-2,3]",
                VType::U8 => "[1,0,3]",
                _ => "[0.5,-2,3.25]",
            };
            assert_eq!(back, want, "{t:?}");
            assert_eq!(VType::parse(t.name()), Some(t));
            // The wrong length isn't that vector.
            assert_eq!(t.decode(&blob, Some(4)), None, "{t:?}");
        }
        assert_eq!(
            VType::F32
                .decode(&vector::to_f32_le(&[0.1, 0.2, 0.3]), None)
                .unwrap(),
            "[0.1,0.2,0.3]"
        );
        assert_eq!(VType::F32.decode(&[1, 2, 3], None), None);
        assert_eq!(VType::F32.decode(&[], None), None);
    }

    #[test]
    fn spellings() {
        assert_eq!(spell(3, VType::F32), "VECTOR(3, FLOAT32)");
        for (s, want) in [
            ("VECTOR(3, FLOAT32)", Some((Some(3), VType::F32))),
            ("vector(1536)", Some((Some(1536), VType::F32))),
            ("vector(2, int8)", Some((Some(2), VType::I8))),
            ("Vector(4, BFLOAT16)", Some((Some(4), VType::Bf16))),
            ("vector<float, 3>", Some((Some(3), VType::F32))),
            ("vector<double, 2>", Some((Some(2), VType::F64))),
            ("VECTOR(FLOAT64, 8)", Some((Some(8), VType::F64))),
            ("VECTOR(*, FLOAT32)", Some((None, VType::F32))),
            ("vector", Some((None, VType::F32))),
            ("halfvec(4)", Some((Some(4), VType::F16))),
            ("F32_BLOB(8)", Some((Some(8), VType::F32))),
            ("vectors", None),
            ("string", None),
            ("vector(3, text)", None),
        ] {
            assert_eq!(parse_spelling(s), want, "{s}");
        }
    }

    #[test]
    fn blob_arguments_parse_back() {
        let blob = VType::F32.encode(&[1.0, 0.1, -2.0]);
        let q = quote_bytes(&blob);
        assert!(q.starts_with('"') && q.contains("\\x"), "{q}");
        let args = crate::command::parse_line(&format!("HSET k f {q} # FLOAT32")).unwrap();
        assert_eq!(args[3], blob);
        assert_eq!(args.len(), 4);
        assert_eq!(quote_bytes(b"a\"\\"), "\"a\\\"\\\\\"");
        assert_eq!(
            pack(&serde_json::json!([1, 2]), VType::F64).unwrap(),
            vector::to_f64_le(&[1.0, 2.0])
        );
        assert_eq!(
            pack(&serde_json::json!("[1,2]"), VType::U8).unwrap(),
            [1, 2]
        );
        assert_eq!(pack(&serde_json::json!("hola"), VType::F32), None);
    }

    /// `FT.INFO` as Redis 8 answers it (RESP2), with a bare flag.
    fn redis8_info() -> Value {
        arr(vec![
            b("index_name"),
            b("docs"),
            b("index_options"),
            arr(vec![]),
            b("index_definition"),
            arr(vec![
                b("key_type"),
                b("HASH"),
                b("prefixes"),
                arr(vec![b("doc:")]),
                b("default_score"),
                b("1"),
            ]),
            b("attributes"),
            arr(vec![
                arr(vec![
                    b("identifier"),
                    b("title"),
                    b("attribute"),
                    b("title"),
                    b("type"),
                    b("TEXT"),
                    b("WEIGHT"),
                    b("2"),
                    b("SORTABLE"),
                ]),
                arr(vec![
                    b("identifier"),
                    b("emb"),
                    b("attribute"),
                    b("emb"),
                    b("type"),
                    b("VECTOR"),
                    b("algorithm"),
                    b("HNSW"),
                    b("data_type"),
                    b("FLOAT32"),
                    b("dim"),
                    Value::Int(3),
                    b("distance_metric"),
                    b("COSINE"),
                    b("M"),
                    Value::Int(8),
                    b("ef_construction"),
                    Value::Int(100),
                    b("ef_runtime"),
                    Value::Int(10),
                ]),
                arr(vec![
                    b("identifier"),
                    b("tag"),
                    b("attribute"),
                    b("lang"),
                    b("type"),
                    b("TAG"),
                    b("SEPARATOR"),
                    b(";"),
                ]),
            ]),
            b("num_docs"),
            Value::Int(0),
        ])
    }

    #[test]
    fn redisearch_info() {
        let ix = FtIndex::from_info("docs", &redis8_info()).unwrap();
        assert!(!ix.on_json);
        assert_eq!(ix.prefixes, ["doc:"]);
        assert_eq!(ix.fields.len(), 3);
        assert_eq!(ix.fields[0].args, ["WEIGHT", "2", "SORTABLE"]);
        let v = ix.vector_field("emb").unwrap();
        assert_eq!((v.dims, v.ty, v.metric.as_str()), (3, VType::F32, "COSINE"));
        assert_eq!(
            v.index(),
            VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some("cosine".into()),
                params: [("m", "8"), ("ef_construction", "100"), ("ef_runtime", "10")]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }
        );
        assert_eq!(ix.fields[2].data_type(), "TAG");
        assert_eq!(ix.fields[1].data_type(), "VECTOR(3, FLOAT32)");
        assert_eq!(
            ix.create_command(),
            "FT.CREATE docs ON HASH PREFIX 1 doc: SCHEMA title TEXT WEIGHT 2 SORTABLE \
             emb VECTOR HNSW 12 TYPE FLOAT32 DIM 3 DISTANCE_METRIC COSINE M 8 EF_CONSTRUCTION 100 EF_RUNTIME 10 \
             tag AS lang TAG SEPARATOR ;"
        );
        assert!(ix.covers(b"doc:1") && !ix.covers(b"other:1"));
        // Back from what the designer stores.
        let again = FtVector::from_index(&v.index(), VType::F32, 3).unwrap();
        assert_eq!(&again, v);
        assert!(FtVector::from_index(
            &VectorIndex {
                metric: Some("l1".into()),
                ..Default::default()
            },
            VType::F32,
            3
        )
        .is_err());
    }

    #[test]
    fn valkey_info() {
        let info = arr(vec![
            b("index_name"),
            b("docs"),
            b("index_definition"),
            arr(vec![
                b("key_type"),
                b("JSON"),
                b("prefixes"),
                arr(vec![b("j:")]),
            ]),
            b("attributes"),
            arr(vec![
                arr(vec![
                    b("identifier"),
                    b("$.v"),
                    b("attribute"),
                    b("v"),
                    b("user_indexed_memory"),
                    Value::Int(0),
                    b("type"),
                    b("VECTOR"),
                    b("index"),
                    arr(vec![
                        b("capacity"),
                        Value::Int(10240),
                        b("dimensions"),
                        Value::Int(2),
                        b("distance_metric"),
                        b("IP"),
                        b("size"),
                        b("0"),
                        b("data_type"),
                        b("FLOAT32"),
                        b("algorithm"),
                        arr(vec![
                            b("name"),
                            b("HNSW"),
                            b("m"),
                            Value::Int(16),
                            b("ef_construction"),
                            Value::Int(200),
                            b("ef_runtime"),
                            Value::Int(10),
                        ]),
                    ]),
                ]),
                arr(vec![
                    b("identifier"),
                    b("$.tag"),
                    b("attribute"),
                    b("tag"),
                    b("user_indexed_memory"),
                    Value::Int(0),
                    b("type"),
                    b("TAG"),
                    b("SEPARATOR"),
                    b(","),
                    b("CASESENSITIVE"),
                    Value::Int(1),
                    b("size"),
                    b("0"),
                ]),
                arr(vec![
                    b("identifier"),
                    b("$.lang"),
                    b("attribute"),
                    b("lang"),
                    b("user_indexed_memory"),
                    Value::Int(0),
                    b("type"),
                    b("TAG"),
                    b("SEPARATOR"),
                    b(","),
                    b("CASESENSITIVE"),
                    Value::Int(0),
                ]),
            ]),
        ]);
        let ix = FtIndex::from_info("docs", &info).unwrap();
        assert!(ix.on_json);
        let v = ix.vector_field("v").unwrap();
        assert_eq!((v.dims, v.metric.as_str()), (2, "IP"));
        assert_eq!(
            ix.create_command(),
            "FT.CREATE docs ON JSON PREFIX 1 j: SCHEMA $.v AS v VECTOR HNSW 14 TYPE FLOAT32 DIM 2 DISTANCE_METRIC IP \
             M 16 EF_CONSTRUCTION 200 EF_RUNTIME 10 INITIAL_CAP 10240 $.tag AS tag TAG CASESENSITIVE $.lang AS lang TAG"
        );
    }

    #[test]
    fn search_results_decode_vectors() {
        let ix = FtIndex::from_info("docs", &redis8_info()).unwrap();
        let blob = VType::F32.encode(&[0.1, 0.2, 0.3]);
        let reply = arr(vec![
            Value::Int(2),
            b("doc:1"),
            arr(vec![
                b("title"),
                b("hola"),
                b("emb"),
                Value::BulkString(blob),
            ]),
            b("doc:2"),
            arr(vec![b("title"), b("chau"), b("__emb_score"), b("0.5")]),
        ]);
        let args: Vec<Vec<u8>> = ["FT.SEARCH", "docs", "*"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let t = search_table(&args, &reply, Some(&ix));
        assert_eq!(t.columns, ["id", "title", "emb", "__emb_score"]);
        assert_eq!(t.rows[0][2], serde_json::json!("[0.1,0.2,0.3]"));
        assert_eq!(t.rows[1][2], serde_json::Value::Null);
        assert_eq!(
            t.vectors[2],
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(t.types[2], "VECTOR(3, FLOAT32)");
        assert!(t.message.unwrap().contains("2 documentos"));
        // WITHSCORES and NOCONTENT.
        let args: Vec<Vec<u8>> = ["FT.SEARCH", "docs", "*", "WITHSCORES", "NOCONTENT"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let t = search_table(
            &args,
            &arr(vec![Value::Int(1), b("doc:1"), b("0.75")]),
            None,
        );
        assert_eq!(t.columns, ["id", "score"]);
        assert_eq!(
            t.rows,
            vec![vec![serde_json::json!("doc:1"), serde_json::json!("0.75")]]
        );
        // JSON vectors come as arrays (DIALECT 3 wraps them once more).
        assert_eq!(json_vector("[[1.5,2]]").as_deref(), Some("[1.5,2]"));
        assert_eq!(json_vector("[1.50, 2]").as_deref(), Some("[1.5,2]"));
        assert_eq!(json_vector("\"x\""), None);
    }

    #[test]
    fn aggregate_rows() {
        let reply = arr(vec![
            Value::Int(2),
            arr(vec![b("lang"), b("es"), b("n"), b("3")]),
            arr(vec![b("lang"), b("en")]),
        ]);
        let t = aggregate_table(&reply);
        assert_eq!(t.columns, ["lang", "n"]);
        assert_eq!(
            t.rows[1],
            vec![serde_json::json!("en"), serde_json::Value::Null]
        );
    }

    #[test]
    fn vector_sets() {
        let v = arr(vec![b("0.09921259433031082"), b("0.30000001192092896")]);
        assert_eq!(vemb_text(&v).as_deref(), Some("[0.099212594,0.3]"));
        assert_eq!(
            vemb_text(&arr(vec![Value::Double(1.0), Value::Double(-0.5)])).as_deref(),
            Some("[1,-0.5]")
        );
        assert_eq!(vemb_text(&Value::Nil), None);
        assert_eq!(quant_flag("int8"), Some("Q8"));
        assert_eq!(quant_flag("f32"), Some("NOQUANT"));
        assert_eq!(quant_flag("bin"), Some("BIN"));
        assert_eq!(
            vadd_args("vs", "a b", &[0.1, 2.0], Some("NOQUANT")),
            ["VADD", "vs", "VALUES", "2", "0.1", "2", "a b", "NOQUANT"]
        );
    }

    #[test]
    fn the_registry_finds_a_hash_field() {
        let mut k = Known::default();
        let ix = FtIndex::from_info("docs", &redis8_info()).unwrap();
        k.indexes.insert("docs".into(), ix);
        assert_eq!(k.hash_vector("doc:7", "emb").unwrap().ty, VType::F32);
        assert!(k.hash_vector("doc:7", "title").is_none());
        assert!(k.hash_vector("x:7", "emb").is_none());
    }
}
