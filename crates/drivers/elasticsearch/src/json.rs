//! A JSON value that keeps object keys in document order. `serde_json`'s
//! `Map` sorts them unless its `preserve_order` feature is on, and turning
//! that on here would change it for the whole workspace. Document columns
//! must follow the order the fields appear in, so responses are parsed into
//! this instead.

use dbine_driver::{json_f64, json_i64, json_u64};
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(serde_json::Number),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

pub type Obj = Vec<(String, J)>;

impl J {
    pub fn parse(text: &str) -> serde_json::Result<J> {
        serde_json::from_str(text)
    }

    pub fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Nested lookup: `at(&["hits", "hits"])`.
    pub fn at(&self, path: &[&str]) -> Option<&J> {
        path.iter().try_fold(self, |v, k| v.get(k))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[J]> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&Obj> {
        match self {
            J::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            J::Num(n) => n.as_u64(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            J::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn is_scalar(&self) -> bool {
        !matches!(self, J::Arr(_) | J::Obj(_))
    }

    /// Text of a scalar (strings unquoted), compact JSON otherwise.
    pub fn text(&self) -> String {
        match self {
            J::Null => String::new(),
            J::Str(s) => s.clone(),
            J::Bool(b) => b.to_string(),
            J::Num(n) => n.to_string(),
            _ => self.compact(),
        }
    }

    pub fn compact(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// A result cell: scalars as they are (big integers as strings),
    /// arrays and objects as compact JSON.
    pub fn cell(&self) -> Value {
        match self {
            J::Null => Value::Null,
            J::Bool(b) => Value::Bool(*b),
            J::Num(n) => {
                if let Some(i) = n.as_i64() {
                    json_i64(i)
                } else if let Some(u) = n.as_u64() {
                    json_u64(u)
                } else {
                    json_f64(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            J::Str(s) => Value::String(s.clone()),
            J::Arr(_) | J::Obj(_) => Value::String(self.compact()),
        }
    }

    /// The inverse of [`J::cell`], for documents written back (insert
    /// scripts): a string holding a JSON object or array (how nested values
    /// are shown) becomes that value again; the rest is kept as is.
    pub fn from_cell(v: &Value) -> J {
        match v {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::Number(n) => J::Num(n.clone()),
            Value::String(s) => {
                let t = s.trim();
                let nested = (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']'));
                nested.then(|| J::parse(t).ok()).flatten().unwrap_or_else(|| J::Str(s.clone()))
            }
            Value::Array(a) => J::Arr(a.iter().map(J::from_cell).collect()),
            Value::Object(o) => J::Obj(o.iter().map(|(k, v)| (k.clone(), J::from_cell(v))).collect()),
        }
    }
}

impl Serialize for J {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            J::Null => s.serialize_unit(),
            J::Bool(b) => s.serialize_bool(*b),
            J::Num(n) => n.serialize(s),
            J::Str(v) => s.serialize_str(v),
            J::Arr(a) => {
                let mut seq = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            J::Obj(o) => {
                let mut map = s.serialize_map(Some(o.len()))?;
                for (k, v) in o {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

struct JVisitor;

impl<'de> Visitor<'de> for JVisitor {
    type Value = J;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_unit<E>(self) -> Result<J, E> {
        Ok(J::Null)
    }
    fn visit_none<E>(self) -> Result<J, E> {
        Ok(J::Null)
    }
    fn visit_bool<E>(self, v: bool) -> Result<J, E> {
        Ok(J::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<J, E> {
        Ok(J::Num(v.into()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<J, E> {
        Ok(J::Num(v.into()))
    }
    fn visit_f64<E>(self, v: f64) -> Result<J, E> {
        Ok(serde_json::Number::from_f64(v).map_or(J::Null, J::Num))
    }
    fn visit_str<E>(self, v: &str) -> Result<J, E> {
        Ok(J::Str(v.to_string()))
    }
    fn visit_string<E>(self, v: String) -> Result<J, E> {
        Ok(J::Str(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<J, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element()? {
            out.push(v);
        }
        Ok(J::Arr(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<J, A::Error> {
        let mut out: Obj = Vec::new();
        while let Some((k, v)) = map.next_entry::<String, J>()? {
            // Duplicate keys: the last one wins, like serde_json.
            if let Some(slot) = out.iter_mut().find(|(ek, _)| *ek == k) {
                slot.1 = v;
            } else {
                out.push((k, v));
            }
        }
        Ok(J::Obj(out))
    }
}

impl<'de> Deserialize<'de> for J {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<J, D::Error> {
        d.deserialize_any(JVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_key_order() {
        let j = J::parse(r#"{"zeta":1,"alpha":{"y":2,"b":[1,"x"]},"mid":null}"#).unwrap();
        let keys: Vec<_> = j.as_obj().unwrap().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["zeta", "alpha", "mid"]);
        assert_eq!(j.get("alpha").unwrap().compact(), r#"{"y":2,"b":[1,"x"]}"#);
        assert_eq!(j.at(&["alpha", "y"]).unwrap().cell(), serde_json::json!(2));
    }

    #[test]
    fn big_numbers_are_strings() {
        let j = J::parse("[9007199254740993, 1.5, 18446744073709551615]").unwrap();
        let a = j.as_arr().unwrap();
        assert_eq!(a[0].cell(), serde_json::json!("9007199254740993"));
        assert_eq!(a[1].cell(), serde_json::json!(1.5));
        assert_eq!(a[2].cell(), serde_json::json!("18446744073709551615"));
    }
}
