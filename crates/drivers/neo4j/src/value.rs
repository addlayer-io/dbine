//! Bolt values as JSON, and JSON as result cells.
//!
//! Graph values take the shape Neptune's openCypher HTTP API uses, so the
//! three engines show the same thing:
//! - node: `{"~id", "~entityType": "node", "~labels": […], "~properties": {…}}`
//! - relationship: `{"~id", "~entityType": "relationship", "~type", "~start", "~end", "~properties": {…}}`
//! - path: `{"~entityType": "path", "~nodes": […], "~relationships": […]}`
//!
//! Ids are the element ids when the server has them (Neo4j 5), the numeric
//! ids otherwise (Memgraph, Neo4j 4). Temporal values become ISO strings,
//! points `{"srid", "x", "y"[, "z"]}`, durations ISO 8601 (`P1M2DT3.5S`).
//!
//! Neo4j 2025's VECTOR (Bolt 6) is `{"~vector": [numbers],
//! "~coordinateType": "FLOAT32"}`: a result column of them reads as the
//! vector text (see the driver's `mark_vectors`), and a node edited as
//! JSON writes it back as `vector(…)`, so it stays a VECTOR and doesn't
//! become a list. A value the negotiated protocol can't carry is
//! `{"~unsupported": type, "~minimumProtocol", "~message"}`.

use crate::packstream::{tag, Value};
use chrono::{DateTime, NaiveDate, NaiveTime};
use dbine_driver::{json_bytes, json_f64, json_i64, json_u64};
use serde_json::{json, Map, Value as Json};

pub fn to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::Float(f) => json_f64(*f),
        Value::Bytes(b) => json_bytes(b),
        Value::String(s) => Json::String(s.clone()),
        Value::List(l) => Json::Array(l.iter().map(to_json).collect()),
        Value::Map(m) => Json::Object(m.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
        Value::Struct(t, f) => structure(*t, f),
    }
}

fn props(v: Option<&Value>) -> Json {
    v.map(to_json).unwrap_or_else(|| json!({}))
}

/// `element_id` (Bolt 5) or the numeric id.
fn id(num: Option<&Value>, element: Option<&Value>) -> Json {
    match (element, num) {
        (Some(Value::String(s)), _) => Json::String(s.clone()),
        (_, Some(v)) => to_json(v),
        _ => Json::Null,
    }
}

fn node(f: &[Value]) -> Json {
    let mut m = Map::new();
    m.insert("~id".into(), id(f.first(), f.get(3)));
    m.insert("~entityType".into(), "node".into());
    m.insert(
        "~labels".into(),
        f.get(1).map(to_json).unwrap_or_else(|| json!([])),
    );
    m.insert("~properties".into(), props(f.get(2)));
    Json::Object(m)
}

fn relationship(f: &[Value]) -> Json {
    let mut m = Map::new();
    m.insert("~id".into(), id(f.first(), f.get(5)));
    m.insert("~entityType".into(), "relationship".into());
    m.insert("~type".into(), f.get(3).map(to_json).unwrap_or(Json::Null));
    m.insert("~start".into(), id(f.get(1), f.get(6)));
    m.insert("~end".into(), id(f.get(2), f.get(7)));
    m.insert("~properties".into(), props(f.get(4)));
    Json::Object(m)
}

/// Path: nodes, unbound relationships and the index sequence that walks
/// them (`[rel_index (1-based, negative = backwards), node_index, …]`).
fn path(f: &[Value]) -> Json {
    let nodes: Vec<Json> = f
        .first()
        .map(Value::as_list)
        .unwrap_or_default()
        .iter()
        .map(to_json)
        .collect();
    let rels: Vec<&Value> = f
        .get(1)
        .map(Value::as_list)
        .unwrap_or_default()
        .iter()
        .collect();
    let seq: Vec<i64> = f
        .get(2)
        .map(Value::as_list)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    let node_id = |n: &Json| n.get("~id").cloned().unwrap_or(Json::Null);
    let mut path_nodes = vec![nodes.first().cloned().unwrap_or(Json::Null)];
    let mut path_rels = Vec::new();
    let mut prev = 0usize;
    for pair in seq.chunks(2) {
        let (ri, ni) = (pair[0], pair.get(1).copied().unwrap_or(0) as usize);
        let Some(Value::Struct(_, rf)) = rels.get((ri.unsigned_abs() as usize).saturating_sub(1))
        else {
            break;
        };
        let next = nodes.get(ni).cloned().unwrap_or(Json::Null);
        let (start, end) = if ri > 0 { (prev, ni) } else { (ni, prev) };
        let mut m = Map::new();
        m.insert("~id".into(), id(rf.first(), rf.get(3)));
        m.insert("~entityType".into(), "relationship".into());
        m.insert("~type".into(), rf.get(1).map(to_json).unwrap_or(Json::Null));
        m.insert(
            "~start".into(),
            nodes.get(start).map(node_id).unwrap_or(Json::Null),
        );
        m.insert(
            "~end".into(),
            nodes.get(end).map(node_id).unwrap_or(Json::Null),
        );
        m.insert("~properties".into(), props(rf.get(2)));
        path_rels.push(Json::Object(m));
        path_nodes.push(next);
        prev = ni;
    }
    json!({ "~entityType": "path", "~nodes": path_nodes, "~relationships": path_rels })
}

const UNIX_EPOCH_DAYS_FROM_CE: i64 = 719_163;

fn date(days: i64) -> Option<NaiveDate> {
    NaiveDate::from_num_days_from_ce_opt(i32::try_from(days + UNIX_EPOCH_DAYS_FROM_CE).ok()?)
}

fn time(nanos: i64) -> Option<NaiveTime> {
    NaiveTime::from_num_seconds_from_midnight_opt(
        (nanos / 1_000_000_000) as u32,
        (nanos % 1_000_000_000) as u32,
    )
}

fn frac(nanos: i64) -> String {
    if nanos == 0 {
        String::new()
    } else {
        format!(".{:09}", nanos).trim_end_matches('0').to_string()
    }
}

fn offset(secs: i64) -> String {
    if secs == 0 {
        return "Z".into();
    }
    let sign = if secs < 0 { '-' } else { '+' };
    let s = secs.abs();
    format!("{sign}{:02}:{:02}", s / 3600, (s % 3600) / 60)
}

fn int(f: &[Value], i: usize) -> i64 {
    f.get(i).and_then(Value::as_i64).unwrap_or(0)
}

fn local_datetime(secs: i64, nanos: i64) -> Option<String> {
    let dt = DateTime::from_timestamp(secs, nanos as u32)?.naive_utc();
    Some(format!("{}{}", dt.format("%Y-%m-%dT%H:%M:%S"), frac(nanos)))
}

/// A Bolt 6 VECTOR (`Vector::Structure(type_marker, data)`, big-endian
/// numbers of one type).
#[derive(Debug, Clone, PartialEq)]
pub struct NativeVector {
    /// Cypher's coordinate type: `FLOAT32`, `FLOAT64`, `INTEGER8`…
    pub coordinate: &'static str,
    pub values: Numbers,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Numbers {
    F32(Vec<f32>),
    F64(Vec<f64>),
    Int(Vec<i64>),
}

impl NativeVector {
    /// From the structure's fields; `None` when they aren't a vector's.
    pub fn decode(f: &[Value]) -> Option<NativeVector> {
        let (Some(Value::Bytes(marker)), Some(Value::Bytes(data))) = (f.first(), f.get(1)) else {
            return None;
        };
        let (coordinate, width) = match marker.as_slice() {
            [0xC8] => ("INTEGER8", 1),
            [0xC9] => ("INTEGER16", 2),
            [0xCA] => ("INTEGER32", 4),
            [0xCB] => ("INTEGER64", 8),
            [0xC6] => ("FLOAT32", 4),
            [0xC1] => ("FLOAT64", 8),
            _ => return None,
        };
        if data.len() % width != 0 {
            return None;
        }
        let chunks = data.chunks_exact(width);
        let values = match marker[0] {
            0xC6 => Numbers::F32(
                chunks
                    .map(|c| f32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            ),
            0xC1 => Numbers::F64(
                chunks
                    .map(|c| f64::from_be_bytes(c.try_into().unwrap_or([0; 8])))
                    .collect(),
            ),
            _ => Numbers::Int(
                chunks
                    .map(|c| match c.len() {
                        1 => i64::from(c[0] as i8),
                        2 => i64::from(i16::from_be_bytes([c[0], c[1]])),
                        4 => i64::from(i32::from_be_bytes([c[0], c[1], c[2], c[3]])),
                        _ => i64::from_be_bytes(c.try_into().unwrap_or([0; 8])),
                    })
                    .collect(),
            ),
        };
        Some(NativeVector { coordinate, values })
    }

    pub fn dims(&self) -> usize {
        match &self.values {
            Numbers::F32(v) => v.len(),
            Numbers::F64(v) => v.len(),
            Numbers::Int(v) => v.len(),
        }
    }

    /// `VECTOR<FLOAT32>(3)`.
    pub fn type_name(&self) -> String {
        format!("VECTOR<{}>({})", self.coordinate, self.dims())
    }

    /// The vector text (`[0.1,2]`, FLOAT32 at its own precision), or
    /// `None` with a NaN or ±∞ (which a VECTOR can't hold anyway).
    pub fn text(&self) -> Option<String> {
        match &self.values {
            Numbers::F32(v) if v.iter().all(|x| x.is_finite()) => {
                Some(dbine_driver::vector::text(v.iter().copied()))
            }
            Numbers::F64(v) if v.iter().all(|x| x.is_finite()) => {
                Some(dbine_driver::vector::text(v.iter().copied()))
            }
            Numbers::Int(v) => Some(dbine_driver::vector::text(v.iter().map(|&x| x as f64))),
            _ => None,
        }
    }

    fn json(&self) -> Json {
        let numbers: Vec<Json> = match &self.values {
            // The shortest text of the 32-bit float, not its widened form.
            Numbers::F32(v) => v
                .iter()
                .map(|x| json_f64(x.to_string().parse().unwrap_or(f64::from(*x))))
                .collect(),
            Numbers::F64(v) => v.iter().map(|x| json_f64(*x)).collect(),
            Numbers::Int(v) => v.iter().map(|x| Json::from(*x)).collect(),
        };
        json!({ "~vector": numbers, "~coordinateType": self.coordinate })
    }
}

/// The vector element of a Cypher coordinate type.
pub fn coordinate_element(coordinate: &str) -> dbine_driver::VectorElement {
    use dbine_driver::VectorElement as E;
    match coordinate.to_ascii_uppercase().as_str() {
        "FLOAT32" => E::F32,
        "FLOAT64" | "FLOAT" => E::F64,
        "INTEGER8" | "INT8" => E::I8,
        _ => E::Other,
    }
}

/// A VECTOR's JSON form (`{"~vector": […], "~coordinateType": …}`) as
/// (coordinate type, numbers).
pub fn native_vector_json(v: &Json) -> Option<(&str, &Vec<Json>)> {
    let o = v.as_object()?;
    if o.len() != 2 {
        return None;
    }
    Some((
        o.get("~coordinateType")?.as_str()?,
        o.get("~vector")?.as_array()?,
    ))
}

fn structure(t: u8, f: &[Value]) -> Json {
    let s = match t {
        tag::VECTOR => {
            if let Some(v) = NativeVector::decode(f) {
                return v.json();
            }
            None
        }
        // What the negotiated protocol can't carry (a type of a newer one).
        tag::UNSUPPORTED => {
            return json!({
                "~unsupported": f.first().map(to_json),
                "~minimumProtocol": format!(
                    "{}.{}",
                    f.get(1).and_then(Value::as_i64).unwrap_or(0),
                    f.get(2).and_then(Value::as_i64).unwrap_or(0)
                ),
                "~message": f.get(3).and_then(|e| e.get("message")).map(to_json),
            })
        }
        tag::NODE => return node(f),
        tag::RELATIONSHIP => return relationship(f),
        tag::PATH => return path(f),
        tag::UNBOUND_RELATIONSHIP => {
            return json!({ "~id": id(f.first(), f.get(3)), "~entityType": "relationship", "~type": f.get(1).map(to_json), "~properties": props(f.get(2)) })
        }
        tag::DATE => date(int(f, 0)).map(|d| d.format("%Y-%m-%d").to_string()),
        tag::LOCAL_TIME => time(int(f, 0)).map(|t| {
            format!(
                "{}{}",
                t.format("%H:%M:%S"),
                frac(int(f, 0) % 1_000_000_000)
            )
        }),
        tag::TIME => time(int(f, 0)).map(|t| {
            format!(
                "{}{}{}",
                t.format("%H:%M:%S"),
                frac(int(f, 0) % 1_000_000_000),
                offset(int(f, 1))
            )
        }),
        tag::LOCAL_DATE_TIME => local_datetime(int(f, 0), int(f, 1)),
        // UTC seconds + offset.
        tag::DATE_TIME => {
            let off = int(f, 2);
            local_datetime(int(f, 0) + off, int(f, 1)).map(|s| format!("{s}{}", offset(off)))
        }
        // Local seconds + offset (Bolt 4).
        tag::LEGACY_DATE_TIME => {
            local_datetime(int(f, 0), int(f, 1)).map(|s| format!("{s}{}", offset(int(f, 2))))
        }
        tag::DATE_TIME_ZONE_ID | tag::LEGACY_DATE_TIME_ZONE_ID => {
            let zone = f.get(2).and_then(Value::as_str).unwrap_or("UTC");
            let local = if t == tag::DATE_TIME_ZONE_ID {
                // UTC seconds: the zone's offset isn't known here, show UTC.
                local_datetime(int(f, 0), int(f, 1)).map(|s| format!("{s}Z"))
            } else {
                local_datetime(int(f, 0), int(f, 1))
            };
            local.map(|s| format!("{s}[{zone}]"))
        }
        tag::DURATION => Some(duration(int(f, 0), int(f, 1), int(f, 2), int(f, 3))),
        tag::POINT_2D => {
            return json!({ "srid": to_json(&f[0]), "x": f.get(1).map(to_json), "y": f.get(2).map(to_json) })
        }
        tag::POINT_3D => {
            return json!({ "srid": to_json(&f[0]), "x": f.get(1).map(to_json), "y": f.get(2).map(to_json), "z": f.get(3).map(to_json) })
        }
        other => {
            return json!({ "~struct": format!("0x{other:02X}"), "fields": f.iter().map(to_json).collect::<Vec<_>>() })
        }
    };
    s.map(Json::String).unwrap_or(Json::Null)
}

pub fn duration(months: i64, days: i64, secs: i64, nanos: i64) -> String {
    let mut s = String::from("P");
    if months / 12 != 0 {
        s.push_str(&format!("{}Y", months / 12));
    }
    if months % 12 != 0 {
        s.push_str(&format!("{}M", months % 12));
    }
    if days != 0 {
        s.push_str(&format!("{days}D"));
    }
    if secs != 0 || nanos != 0 || s == "P" {
        s.push('T');
        let (h, m, sec) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        if h != 0 {
            s.push_str(&format!("{h}H"));
        }
        if m != 0 {
            s.push_str(&format!("{m}M"));
        }
        if sec != 0 || nanos != 0 || (h == 0 && m == 0) {
            s.push_str(&format!("{sec}{}S", frac(nanos)));
        }
    }
    s
}

/// A value as a grid cell: nested values as compact JSON, big integers as strings.
pub fn cell(v: &Json) -> Json {
    match v {
        Json::Object(_) | Json::Array(_) => Json::String(v.to_string()),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                json_i64(i)
            } else if let Some(u) = n.as_u64() {
                json_u64(u)
            } else {
                json_f64(n.as_f64().unwrap_or(0.0))
            }
        }
        other => other.clone(),
    }
}

/// [`type_name`], with a Neo4j 2025 VECTOR (its JSON form) as its own
/// type: `VECTOR<FLOAT32>(3)`.
pub fn type_text(v: &Json) -> String {
    match native_vector_json(v) {
        Some((c, numbers)) if !c.is_empty() && c.chars().all(|x| x.is_ascii_alphanumeric()) => {
            format!("VECTOR<{c}>({})", numbers.len())
        }
        _ => type_name(v).to_string(),
    }
}

/// Type name of a property value as Cypher calls it.
pub fn type_name(v: &Json) -> &'static str {
    match v {
        Json::Null => "NULL",
        Json::Bool(_) => "BOOLEAN",
        Json::Number(n) if n.is_f64() => "FLOAT",
        Json::Number(_) => "INTEGER",
        Json::String(_) => "STRING",
        // Floats only: what a vector index indexes (an embedding).
        Json::Array(a)
            if !a.is_empty()
                && a.iter()
                    .all(|x| x.as_number().is_some_and(serde_json::Number::is_f64)) =>
        {
            "LIST<FLOAT>"
        }
        Json::Array(_) => "LIST",
        Json::Object(o) if o.contains_key("srid") => "POINT",
        Json::Object(_) => "MAP",
    }
}

#[cfg(test)]
mod tests {
    /// Bolt 6 VECTOR structures (big-endian numbers after a type marker)
    /// for each coordinate type, and the unsupported-type placeholder.
    #[test]
    fn bolt6_vectors() {
        let v = |marker: u8, data: Vec<u8>| {
            Value::Struct(
                tag::VECTOR,
                vec![Value::Bytes(vec![marker]), Value::Bytes(data)],
            )
        };
        let f32s: Vec<u8> = [0.1f32, -2.5, 3.0]
            .iter()
            .flat_map(|x| x.to_be_bytes())
            .collect();
        let j = to_json(&v(0xC6, f32s.clone()));
        assert_eq!(
            j,
            json!({"~vector": [0.1, -2.5, 3.0], "~coordinateType": "FLOAT32"})
        );
        let n = NativeVector::decode(&[Value::Bytes(vec![0xC6]), Value::Bytes(f32s)]).unwrap();
        assert_eq!(n.type_name(), "VECTOR<FLOAT32>(3)");
        assert_eq!(n.text().as_deref(), Some("[0.1,-2.5,3]"));
        assert_eq!(
            coordinate_element(n.coordinate),
            dbine_driver::VectorElement::F32
        );
        let f64s: Vec<u8> = [1e-7f64, 2.0]
            .iter()
            .flat_map(|x| x.to_be_bytes())
            .collect();
        let n = NativeVector::decode(&[Value::Bytes(vec![0xC1]), Value::Bytes(f64s)]).unwrap();
        assert_eq!(
            (n.coordinate, n.text().unwrap().as_str()),
            ("FLOAT64", "[0.0000001,2]")
        );
        let n =
            NativeVector::decode(&[Value::Bytes(vec![0xC8]), Value::Bytes(vec![1, 0xFF, 0x80])])
                .unwrap();
        assert_eq!(
            (n.coordinate, n.text().unwrap().as_str()),
            ("INTEGER8", "[1,-1,-128]")
        );
        let n = NativeVector::decode(&[
            Value::Bytes(vec![0xC9]),
            Value::Bytes(vec![0xFF, 0xFE, 0, 7]),
        ])
        .unwrap();
        assert_eq!(n.text().as_deref(), Some("[-2,7]"));
        let n = NativeVector::decode(&[
            Value::Bytes(vec![0xCA]),
            Value::Bytes(70000i32.to_be_bytes().to_vec()),
        ])
        .unwrap();
        assert_eq!(
            (n.type_name().as_str(), n.text().unwrap().as_str()),
            ("VECTOR<INTEGER32>(1)", "[70000]")
        );
        let n = NativeVector::decode(&[
            Value::Bytes(vec![0xCB]),
            Value::Bytes((-5i64).to_be_bytes().to_vec()),
        ])
        .unwrap();
        assert_eq!(n.text().as_deref(), Some("[-5]"));
        // A bad marker or a short payload isn't a vector.
        assert!(NativeVector::decode(&[Value::Bytes(vec![0x01]), Value::Bytes(vec![])]).is_none());
        assert!(
            NativeVector::decode(&[Value::Bytes(vec![0xC6]), Value::Bytes(vec![1, 2])]).is_none()
        );
        // NaN has no vector text.
        let nan: Vec<u8> = f32::NAN.to_be_bytes().to_vec();
        assert!(
            NativeVector::decode(&[Value::Bytes(vec![0xC6]), Value::Bytes(nan)])
                .unwrap()
                .text()
                .is_none()
        );
        let j = json!({"~vector": [1, 2], "~coordinateType": "INTEGER8"});
        assert_eq!(
            native_vector_json(&j).map(|(c, v)| (c, v.len())),
            Some(("INTEGER8", 2))
        );
        assert!(native_vector_json(&json!({"~vector": [1]})).is_none());

        let u = to_json(&Value::Struct(
            tag::UNSUPPORTED,
            vec![
                "UUID".into(),
                Value::Int(6),
                Value::Int(1),
                map([("message", "needs 6.1".into())]),
            ],
        ));
        assert_eq!(u["~unsupported"], json!("UUID"));
        assert_eq!(u["~minimumProtocol"], json!("6.1"));
    }

    use super::*;
    use crate::packstream::map;

    #[test]
    fn graph_values() {
        let n = Value::Struct(
            tag::NODE,
            vec![
                Value::Int(7),
                Value::List(vec!["Person".into()]),
                map([("name", "Ann".into())]),
                "4:x:7".into(),
            ],
        );
        assert_eq!(
            to_json(&n),
            json!({ "~id": "4:x:7", "~entityType": "node", "~labels": ["Person"], "~properties": { "name": "Ann" } })
        );
        let r = Value::Struct(
            tag::RELATIONSHIP,
            vec![
                Value::Int(1),
                Value::Int(7),
                Value::Int(8),
                "KNOWS".into(),
                map([]),
            ],
        );
        assert_eq!(to_json(&r)["~start"], json!(7));
        assert_eq!(to_json(&r)["~type"], json!("KNOWS"));
        // (a)-[:R]->(b)<-[:R]-(c)
        let nodes = Value::List(vec![
            Value::Struct(tag::NODE, vec![Value::Int(1), Value::List(vec![]), map([])]),
            Value::Struct(tag::NODE, vec![Value::Int(2), Value::List(vec![]), map([])]),
            Value::Struct(tag::NODE, vec![Value::Int(3), Value::List(vec![]), map([])]),
        ]);
        let rels = Value::List(vec![
            Value::Struct(
                tag::UNBOUND_RELATIONSHIP,
                vec![Value::Int(10), "R".into(), map([])],
            ),
            Value::Struct(
                tag::UNBOUND_RELATIONSHIP,
                vec![Value::Int(11), "R".into(), map([])],
            ),
        ]);
        let p = to_json(&Value::Struct(
            tag::PATH,
            vec![
                nodes,
                rels,
                Value::List(vec![1.into(), 1.into(), (-2).into(), 2.into()]),
            ],
        ));
        assert_eq!(p["~nodes"].as_array().unwrap().len(), 3);
        assert_eq!(p["~relationships"][0]["~start"], json!(1));
        assert_eq!(p["~relationships"][1]["~start"], json!(3));
        assert_eq!(p["~relationships"][1]["~end"], json!(2));
    }

    #[test]
    fn temporal_values() {
        let s =
            |t, f: Vec<i64>| to_json(&Value::Struct(t, f.into_iter().map(Value::Int).collect()));
        assert_eq!(s(tag::DATE, vec![19_723]), json!("2024-01-01"));
        assert_eq!(
            s(tag::LOCAL_TIME, vec![3_600_000_000_000 + 500_000_000]),
            json!("01:00:00.5")
        );
        assert_eq!(s(tag::TIME, vec![0, -10_800]), json!("00:00:00-03:00"));
        assert_eq!(
            s(tag::LOCAL_DATE_TIME, vec![1_704_067_200, 0]),
            json!("2024-01-01T00:00:00")
        );
        assert_eq!(
            s(tag::DATE_TIME, vec![1_704_067_200, 0, 3_600]),
            json!("2024-01-01T01:00:00+01:00")
        );
        assert_eq!(
            s(tag::LEGACY_DATE_TIME, vec![1_704_067_200, 0, 3_600]),
            json!("2024-01-01T00:00:00+01:00")
        );
        assert_eq!(
            s(tag::DURATION, vec![14, 3, 3_725, 500_000_000]),
            json!("P1Y2M3DT1H2M5.5S")
        );
        assert_eq!(duration(0, 0, 0, 0), "PT0S");
    }

    #[test]
    fn cells() {
        assert_eq!(cell(&json!({ "a": [1] })), json!("{\"a\":[1]}"));
        assert_eq!(
            cell(&json!(9_007_199_254_740_993_i64)),
            json!("9007199254740993")
        );
        assert_eq!(type_name(&json!(1.5)), "FLOAT");
    }
}
