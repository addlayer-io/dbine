//! Vectors (embeddings): what DBine knows about a vector column, and the
//! helpers drivers share to read and write them.
//!
//! One form travels everywhere, whatever the engine's wire format:
//!
//! - **Results:** a vector cell is text with a JSON array of numbers
//!   (`[0.1,-2,3.5]`), whole (never cut like binary). The column says it
//!   holds vectors ([`crate::ResultColumn::vector`]). Sparse vectors keep the
//!   engine's own text (`{1:0.5,4:2}/5`, `[5,[1,4],[0.5,2]]`).
//! - **Edit and insert scripts:** a vector value is a JSON array of numbers
//!   ([`serde_json::Value::Array`]). Each driver writes it as its engine's
//!   vector literal (`'[1,2]'`, `STRING_TO_VECTOR('[1,2]')`,
//!   `VEC_FromText('[1,2]')`, `vector32('[1,2]')`…).
//! - **Transfers:** a vector cell is [`crate::transfer::Cell::Text`] (or
//!   `Json`) with that JSON array. Binary-wire sources decode to it, and each
//!   target encodes it for its column's type.
//!
//! Vector indexes are described by [`VectorIndex`], in
//! [`crate::IndexDef::vector`], with the standard metric names of
//! [`metric`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The type of a vector's numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorElement {
    /// 32-bit float, the usual one.
    #[default]
    F32,
    F64,
    /// IEEE half precision.
    F16,
    /// bfloat16.
    Bf16,
    I8,
    U8,
    /// One bit per dimension (binary vectors).
    Bit,
    /// An element type this build doesn't know.
    #[serde(other)]
    Other,
}

/// What a result column (or a table column) knows about the vectors in it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorMeta {
    /// Declared dimensions (`None`: any length, or not known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dims: Option<u32>,
    #[serde(default)]
    pub element: VectorElement,
    /// A sparse vector (only the non-zero positions are stored). Its cells
    /// keep the engine's text.
    #[serde(default, skip_serializing_if = "is_false")]
    pub sparse: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl VectorMeta {
    /// A dense vector of `element`s.
    pub fn dense(dims: Option<u32>, element: VectorElement) -> Self {
        Self {
            dims,
            element,
            sparse: false,
        }
    }

    /// A sparse vector of `element`s.
    pub fn sparse(dims: Option<u32>, element: VectorElement) -> Self {
        Self {
            dims,
            element,
            sparse: true,
        }
    }
}

/// A vector index: how the engine searches it. The rest of the index
/// (name, column, filter, other options) is the [`crate::IndexDef`]'s.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorIndex {
    /// The algorithm as the engine names it (`hnsw`, `ivfflat`, `diskann`,
    /// `scann`, `ivf`, `flat`…), lower case where the engine doesn't care.
    /// `None`: the engine has one, or picks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,
    /// The distance, with a standard name of [`metric`] when one fits
    /// (`cosine`, `l2`, `ip`…), otherwise the engine's own. `None`: the
    /// engine's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    /// Build parameters as the engine names them (`m`, `ef_construction`,
    /// `lists`, `neighbors`, `quantization`…), with their values as text.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
}

/// Standard distance names for [`VectorIndex::metric`] and the designer's
/// metric list ([`crate::VectorIndexSpec::metrics`]). The UI shows these
/// translated; any other name is shown as the engine spells it.
pub mod metric {
    pub const COSINE: &str = "cosine";
    /// Euclidean distance.
    pub const L2: &str = "l2";
    /// Inner (dot) product.
    pub const IP: &str = "ip";
    /// Manhattan (taxicab) distance.
    pub const L1: &str = "l1";
    pub const HAMMING: &str = "hamming";
    pub const JACCARD: &str = "jaccard";
    pub const ALL: &[&str] = &[COSINE, L2, IP, L1, HAMMING, JACCARD];
}

/// Standard build-parameter names for [`VectorIndex::params`] and the
/// designer's parameter fields. The UI shows these with a translated label;
/// any other name is shown as the engine spells it.
pub mod param {
    /// HNSW: links per node.
    pub const M: &str = "m";
    /// HNSW: candidate list size while building.
    pub const EF_CONSTRUCTION: &str = "ef_construction";
    /// IVF: number of lists (partitions).
    pub const LISTS: &str = "lists";
    /// How vectors are compressed in the index (`int8`, `binary`, `pq`…).
    pub const QUANTIZATION: &str = "quantization";
    pub const ALL: &[&str] = &[M, EF_CONSTRUCTION, LISTS, QUANTIZATION];
}

/// A vector's numbers as its result / transfer text: a JSON array, each
/// number written the shortest way that reads back the same. A value that
/// isn't finite (NaN, ∞) is written `null`.
pub fn text<I>(values: I) -> String
where
    I: IntoIterator,
    I::Item: Into<f64> + Copy + std::fmt::Display,
{
    use std::fmt::Write;
    let mut s = String::from("[");
    for (i, v) in values.into_iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        if v.into().is_finite() {
            let _ = write!(s, "{v}");
        } else {
            s.push_str("null");
        }
    }
    s.push(']');
    s
}

/// [`text`] as a result cell.
pub fn json<I>(values: I) -> serde_json::Value
where
    I: IntoIterator,
    I::Item: Into<f64> + Copy + std::fmt::Display,
{
    serde_json::Value::String(text(values))
}

/// The numbers of a vector written as a JSON array (`[1, 2.5, -3e-2]`),
/// or `None` when the text isn't one. Every number must be finite.
pub fn parse(text: &str) -> Option<Vec<f64>> {
    let inner = text.trim().strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|n| {
            let n = n.trim();
            // `inf`, `NaN` and `+1` parse in Rust but aren't JSON numbers.
            if n.is_empty()
                || n.starts_with('+')
                || n.chars()
                    .any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E')
            {
                return None;
            }
            n.parse::<f64>().ok().filter(|v| v.is_finite())
        })
        .collect()
}

/// A vector value of an edit, insert or transfer: a JSON array of numbers,
/// or text holding one. `None` for anything else.
pub fn parse_value(v: &serde_json::Value) -> Option<Vec<f64>> {
    match v {
        serde_json::Value::Array(items) => items
            .iter()
            .map(|n| n.as_f64().filter(|v| v.is_finite()))
            .collect(),
        serde_json::Value::String(s) => parse(s),
        _ => None,
    }
}

/// A vector value of an edit as its compact JSON text (`[1,2.5]`), the
/// form most engines take inside a string literal. `None` when `v` isn't a
/// vector ([`parse_value`]).
pub fn value_text(v: &serde_json::Value) -> Option<String> {
    parse_value(v).map(text)
}

/// Little-endian 32-bit floats (MySQL 9, MariaDB, libSQL `F32_BLOB`,
/// sqlite-vec, Redis `FLOAT32`…). `None` when the length isn't a multiple
/// of 4.
pub fn from_f32_le(b: &[u8]) -> Option<Vec<f32>> {
    let (chunks, rest) = b.as_chunks::<4>();
    rest.is_empty()
        .then(|| chunks.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

/// Little-endian 64-bit floats. `None` when the length isn't a multiple
/// of 8.
pub fn from_f64_le(b: &[u8]) -> Option<Vec<f64>> {
    let (chunks, rest) = b.as_chunks::<8>();
    rest.is_empty()
        .then(|| chunks.iter().map(|c| f64::from_le_bytes(*c)).collect())
}

/// Little-endian IEEE half floats. `None` when the length is odd.
pub fn from_f16_le(b: &[u8]) -> Option<Vec<f32>> {
    let (chunks, rest) = b.as_chunks::<2>();
    rest.is_empty().then(|| {
        chunks
            .iter()
            .map(|c| f16_to_f32(u16::from_le_bytes(*c)))
            .collect()
    })
}

/// Little-endian bfloat16. `None` when the length is odd.
pub fn from_bf16_le(b: &[u8]) -> Option<Vec<f32>> {
    let (chunks, rest) = b.as_chunks::<2>();
    rest.is_empty().then(|| {
        chunks
            .iter()
            .map(|c| f32::from_bits((u16::from_le_bytes(*c) as u32) << 16))
            .collect()
    })
}

/// Signed bytes.
pub fn from_i8(b: &[u8]) -> Vec<f32> {
    b.iter().map(|&x| x as i8 as f32).collect()
}

/// Unsigned bytes.
pub fn from_u8(b: &[u8]) -> Vec<f32> {
    b.iter().map(|&x| x as f32).collect()
}

/// Packed bits, most significant bit first, as 0 and 1. `dims` cuts the
/// padding of the last byte (`None`: every bit).
pub fn from_bits(b: &[u8], dims: Option<usize>) -> Vec<u8> {
    let n = dims.unwrap_or(b.len() * 8).min(b.len() * 8);
    (0..n).map(|i| (b[i / 8] >> (7 - i % 8)) & 1).collect()
}

/// Little-endian 32-bit floats (what [`from_f32_le`] reads).
pub fn to_f32_le(values: &[f64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|&v| (v as f32).to_le_bytes())
        .collect()
}

/// Little-endian 64-bit floats.
pub fn to_f64_le(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|&v| v.to_le_bytes()).collect()
}

/// Little-endian IEEE half floats, rounded to nearest even.
pub fn to_f16_le(values: &[f64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|&v| f32_to_f16(v as f32).to_le_bytes())
        .collect()
}

/// Little-endian bfloat16, rounded to nearest even.
pub fn to_bf16_le(values: &[f64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|&v| f32_to_bf16(v as f32).to_le_bytes())
        .collect()
}

/// Signed bytes; values out of range are clamped, fractions rounded.
pub fn to_i8(values: &[f64]) -> Vec<u8> {
    values
        .iter()
        .map(|&v| v.round().clamp(-128.0, 127.0) as i8 as u8)
        .collect()
}

/// Unsigned bytes; values out of range are clamped, fractions rounded.
pub fn to_u8(values: &[f64]) -> Vec<u8> {
    values
        .iter()
        .map(|&v| v.round().clamp(0.0, 255.0) as u8)
        .collect()
}

/// Packed bits, most significant bit first: every value other than 0 is a
/// 1. The last byte is padded with zeros.
pub fn to_bits(values: &[f64]) -> Vec<u8> {
    let mut out = vec![0u8; values.len().div_ceil(8)];
    for (i, &v) in values.iter().enumerate() {
        if v != 0.0 {
            out[i / 8] |= 1 << (7 - i % 8);
        }
    }
    out
}

/// An IEEE half float as a 32-bit one.
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = match exp {
        0 if frac == 0 => sign,
        0 => {
            // Subnormal: shift until the implicit bit shows.
            let mut e = 127 - 15 + 1;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((f & 0x3ff) << 13)
        }
        0x1f => sign | 0x7f80_0000 | (frac << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}

/// A 32-bit float as an IEEE half float, rounded to nearest even.
pub fn f32_to_f16(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = ((x >> 23) & 0xff) as i32;
    let man = x & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = 1u32 << (shift - 1);
        let mut h = m >> shift;
        let rem = m & ((1 << shift) - 1);
        if rem > half || (rem == half && h & 1 == 1) {
            h += 1;
        }
        return sign | h as u16;
    }
    let mut h = ((e as u32) << 10) | (man >> 13);
    let rem = man & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && h & 1 == 1) {
        // A carry into the exponent is right: it rounds up to the next
        // power of two, or to infinity.
        h += 1;
    }
    sign | h as u16
}

/// A 32-bit float as bfloat16, rounded to nearest even.
pub fn f32_to_bf16(v: f32) -> u16 {
    if v.is_nan() {
        return 0x7fc0 | ((v.to_bits() >> 16) as u16 & 0x8000);
    }
    let x = v.to_bits();
    ((x + 0x7fff + ((x >> 16) & 1)) >> 16) as u16
}

/// Columns of a result that hold vectors, by position.
pub fn columns(columns: &[crate::ResultColumn]) -> Vec<usize> {
    columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.vector.as_ref().is_some_and(|v| !v.sparse))
        .map(|(i, _)| i)
        .collect()
}

/// Rows of a result with their dense vector cells turned into JSON arrays
/// of numbers, the form [`crate::Driver::insert_script`] and
/// [`crate::Driver::update_script`] take for vectors. Cells that don't
/// read as a vector are left as they are.
pub fn rows_for_scripts(
    columns: &[crate::ResultColumn],
    rows: &[Vec<serde_json::Value>],
) -> Vec<Vec<serde_json::Value>> {
    let vec_cols = self::columns(columns);
    rows.iter()
        .map(|r| {
            let mut r = r.clone();
            for &i in &vec_cols {
                if let Some(v) = r.get(i).and_then(parse_value) {
                    r[i] = serde_json::Value::Array(
                        v.into_iter().map(crate::model::json_f64).collect(),
                    );
                }
            }
            r
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_is_a_short_json_array() {
        assert_eq!(text([0.1f32, -2.0, 3.5]), "[0.1,-2,3.5]");
        assert_eq!(text([1.0f64, f64::NAN, f64::INFINITY]), "[1,null,null]");
        assert_eq!(text(Vec::<f32>::new()), "[]");
        assert_eq!(json([1u8, 0]), json!("[1,0]"));
        // It reads back as the same numbers.
        let v = [0.1f32, 1e-7, 123456.79];
        let back = parse(&text(v)).unwrap();
        let back: Vec<f32> = back.into_iter().map(|x| x as f32).collect();
        assert_eq!(back, v);
    }

    #[test]
    fn parse_takes_json_arrays_only() {
        assert_eq!(parse(" [1, 2.5 ,-3e-2] "), Some(vec![1.0, 2.5, -0.03]));
        assert_eq!(parse("[]"), Some(vec![]));
        for bad in [
            "", "1,2", "[1,,2]", "[1,a]", "[inf]", "[NaN]", "[+1]", "{1:2}/3", "[1,null]",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(parse_value(&json!([1, 2.5])), Some(vec![1.0, 2.5]));
        assert_eq!(parse_value(&json!("[1,2]")), Some(vec![1.0, 2.0]));
        assert_eq!(parse_value(&json!([1, "a"])), None);
        assert_eq!(parse_value(&json!(3)), None);
        assert_eq!(value_text(&json!([1.0, 2])).as_deref(), Some("[1,2]"));
    }

    #[test]
    fn binary_forms_round_trip() {
        let v = [1.0, -2.5, 0.125, 3.0];
        assert_eq!(
            from_f32_le(&to_f32_le(&v)).unwrap(),
            [1.0f32, -2.5, 0.125, 3.0]
        );
        assert_eq!(from_f64_le(&to_f64_le(&v)).unwrap(), v);
        assert_eq!(
            from_f16_le(&to_f16_le(&v)).unwrap(),
            [1.0f32, -2.5, 0.125, 3.0]
        );
        assert_eq!(
            from_bf16_le(&to_bf16_le(&v)).unwrap(),
            [1.0f32, -2.5, 0.125, 3.0]
        );
        assert_eq!(
            from_i8(&to_i8(&[-128.0, 5.4, 300.0])),
            [-128.0f32, 5.0, 127.0]
        );
        assert_eq!(from_u8(&to_u8(&[-1.0, 5.6, 300.0])), [0.0f32, 6.0, 255.0]);
        assert_eq!(
            to_bits(&[1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]),
            [0b1011_0000, 0b1000_0000]
        );
        assert_eq!(
            from_bits(&[0b1011_0000, 0b1000_0000], Some(9)),
            [1, 0, 1, 1, 0, 0, 0, 0, 1]
        );
        assert_eq!(from_bits(&[0b1000_0001], None).len(), 8);
        // Lengths that can't be the format.
        assert_eq!(from_f32_le(&[0; 5]), None);
        assert_eq!(from_f64_le(&[0; 12]), None);
        assert_eq!(from_f16_le(&[0; 3]), None);
        assert_eq!(from_bf16_le(&[0; 1]), None);
    }

    #[test]
    fn half_floats_cover_the_edges() {
        // Subnormals, the largest finite, infinity, NaN, signed zero.
        for h in [
            0x0001u16, 0x03ff, 0x0400, 0x7bff, 0x7c00, 0xfc00, 0x8000, 0x3c00, 0xc000,
        ] {
            assert_eq!(f32_to_f16(f16_to_f32(h)), h, "{h:#06x}");
        }
        assert!(f16_to_f32(0x7e00).is_nan());
        assert_eq!(f16_to_f32(0x0001), 2f32.powi(-24));
        // Rounding: halfway rounds to even; overflow goes to infinity.
        assert_eq!(f32_to_f16(1.0 + 2f32.powi(-11)), 0x3c00);
        assert_eq!(f32_to_f16(1.0 + 3.0 * 2f32.powi(-11)), 0x3c02);
        assert_eq!(f32_to_f16(70000.0), 0x7c00);
        assert_eq!(f32_to_f16(1e-9), 0);
        assert_eq!(f32_to_bf16(1.0), 0x3f80);
        assert!(f32::from_bits((f32_to_bf16(f32::NAN) as u32) << 16).is_nan());
    }

    #[test]
    fn script_rows_get_arrays_in_vector_columns() {
        let cols = vec![
            crate::ResultColumn::new("id", "int"),
            crate::ResultColumn::new("v", "vector(2)")
                .with_vector(VectorMeta::dense(Some(2), VectorElement::F32)),
            crate::ResultColumn::new("s", "sparsevec(5)")
                .with_vector(VectorMeta::sparse(Some(5), VectorElement::F32)),
        ];
        let rows = vec![
            vec![json!(1), json!("[1,2.5]"), json!("{1:1}/5")],
            vec![json!(2), json!(null), json!(null)],
        ];
        assert_eq!(columns(&cols), [1]);
        let out = rows_for_scripts(&cols, &rows);
        assert_eq!(out[0], vec![json!(1), json!([1.0, 2.5]), json!("{1:1}/5")]);
        assert_eq!(out[1], rows[1]);
    }

    #[test]
    fn metadata_stays_small_in_json() {
        let m = VectorMeta::dense(Some(3), VectorElement::F32);
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"dims": 3, "element": "f32"})
        );
        let back: VectorMeta = serde_json::from_value(json!({"element": "f4"})).unwrap();
        assert_eq!(back.element, VectorElement::Other);
        assert_eq!(back.dims, None);
        let i: VectorIndex = serde_json::from_value(json!({})).unwrap();
        assert_eq!(i, VectorIndex::default());
        assert_eq!(serde_json::to_value(&i).unwrap(), json!({}));
    }
}
