//! BSON → result cells, the union of document keys, and observed field types.

use chrono::{DateTime, Utc};
use dbine_driver::{json_bytes, json_f64, json_i64, ColumnInfo};
use mongodb::bson::{spec::BinarySubtype, Bson, Document};
use serde_json::{Map, Value};

/// Top-level keys of `docs`, `_id` first, then in order of appearance.
pub fn union_keys<'a>(docs: impl IntoIterator<Item = &'a Document>) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut has_id = false;
    for d in docs {
        for k in d.keys() {
            if k == "_id" {
                has_id = true;
            } else if !keys.iter().any(|x| x == k) {
                keys.push(k.clone());
            }
        }
    }
    if has_id {
        keys.insert(0, "_id".into());
    }
    keys
}

/// A cell: scalars as JSON scalars, nested documents and arrays as compact
/// JSON text (see [`plain`]).
pub fn cell(b: &Bson) -> Value {
    match b {
        Bson::Document(_) | Bson::Array(_) => Value::String(plain(b).to_string()),
        other => plain(other),
    }
}

/// BSON as plain JSON: ObjectId → hex, dates → ISO text, big integers and
/// decimals → strings, binary → `0x…` (UUIDs in their usual form).
pub fn plain(b: &Bson) -> Value {
    match b {
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Boolean(v) => Value::Bool(*v),
        Bson::Int32(v) => Value::from(*v),
        Bson::Int64(v) => json_i64(*v),
        Bson::Double(v) => json_f64(*v),
        Bson::String(s) | Bson::Symbol(s) | Bson::JavaScriptCode(s) => Value::String(s.clone()),
        Bson::ObjectId(o) => Value::String(o.to_hex()),
        Bson::DateTime(d) => Value::String(iso(d.timestamp_millis())),
        Bson::Decimal128(d) => Value::String(d.to_string()),
        Bson::Timestamp(t) => Value::String(format!("Timestamp({}, {})", t.time, t.increment)),
        Bson::RegularExpression(r) => Value::String(format!("/{}/{}", r.pattern, r.options)),
        Bson::Binary(bin) if bin.subtype == BinarySubtype::Uuid && bin.bytes.len() == 16 => {
            let h: String = bin.bytes.iter().map(|x| format!("{x:02x}")).collect();
            Value::String(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
        }
        Bson::Binary(bin) => json_bytes(&bin.bytes),
        Bson::Document(d) => Value::Object(d.iter().map(|(k, v)| (k.clone(), plain(v))).collect::<Map<_, _>>()),
        Bson::Array(a) => Value::Array(a.iter().map(plain).collect()),
        Bson::MinKey => Value::String("MinKey".into()),
        Bson::MaxKey => Value::String("MaxKey".into()),
        other => Value::String(other.to_string()),
    }
}

/// `2024-01-31 13:45:00` (UTC), with milliseconds when there are any.
pub fn iso(ms: i64) -> String {
    match DateTime::<Utc>::from_timestamp_millis(ms) {
        Some(d) if ms % 1000 == 0 => d.format("%Y-%m-%d %H:%M:%S").to_string(),
        Some(d) => d.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        None => ms.to_string(),
    }
}

/// The shell's name for a BSON type.
pub fn type_name(b: &Bson) -> &'static str {
    match b {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(_) => "binData",
        Bson::Undefined => "undefined",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "date",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "javascript",
        Bson::Symbol(_) => "symbol",
        Bson::Int32(_) => "int",
        Bson::Timestamp(_) => "timestamp",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        Bson::MinKey => "minKey",
        Bson::MaxKey => "maxKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

/// Fields inferred from a sample: the types seen (most frequent first,
/// joined with `|`), nullable unless present and non-null in every document.
pub fn infer_columns(docs: &[Document]) -> Vec<ColumnInfo> {
    union_keys(docs)
        .into_iter()
        .map(|name| {
            let mut types: Vec<(&'static str, usize)> = Vec::new();
            let mut present = 0;
            let mut null_seen = false;
            for d in docs {
                if let Some(v) = d.get(&name) {
                    present += 1;
                    let t = type_name(v);
                    null_seen |= matches!(v, Bson::Null | Bson::Undefined);
                    if !matches!(v, Bson::Null | Bson::Undefined) {
                        match types.iter_mut().find(|(n, _)| *n == t) {
                            Some(e) => e.1 += 1,
                            None => types.push((t, 1)),
                        }
                    }
                }
            }
            types.sort_by(|a, b| b.1.cmp(&a.1));
            let data_type = if types.is_empty() { "null".into() } else { types.iter().map(|t| t.0).collect::<Vec<_>>().join("|") };
            ColumnInfo {
                primary_key: name == "_id",
                nullable: present < docs.len() || null_seen,
                auto_increment: false,
                default_value: None,
                data_type,
                name,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::{doc, oid::ObjectId, DateTime as BDate};

    #[test]
    fn keys_union_puts_id_first() {
        let a = doc! { "name": "x", "_id": 1 };
        let b = doc! { "age": 3, "name": "y" };
        assert_eq!(union_keys([&a, &b]), vec!["_id", "name", "age"]);
    }

    #[test]
    fn cells_flatten_nested_and_special_types() {
        let oid = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        assert_eq!(cell(&Bson::ObjectId(oid)), Value::String("65a1b2c3d4e5f60718293a4b".into()));
        assert_eq!(cell(&Bson::DateTime(BDate::from_millis(1_706_708_700_000))), Value::String("2024-01-31 13:45:00".into()));
        let nested = Bson::Document(doc! { "a": [1, { "b": oid }] });
        assert_eq!(cell(&nested), Value::String(r#"{"a":[1,{"b":"65a1b2c3d4e5f60718293a4b"}]}"#.into()));
        assert_eq!(cell(&Bson::Int64(1 << 60)), Value::String((1_i64 << 60).to_string()));
    }

    #[test]
    fn inferred_types_and_nullability() {
        let docs = vec![doc! { "_id": 1, "a": "x", "b": 1 }, doc! { "_id": 2, "a": 5, "b": 2 }, doc! { "_id": 3, "a": "y" }];
        let cols = infer_columns(&docs);
        assert_eq!(cols[0].name, "_id");
        assert!(cols[0].primary_key && !cols[0].nullable);
        assert_eq!(cols[1].data_type, "string|int");
        assert!(!cols[1].nullable);
        assert!(cols[2].nullable);
    }
}
