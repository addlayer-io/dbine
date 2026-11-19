//! SAP HANA Cloud's `REAL_VECTOR(n)` and its HNSW vector indexes.
//!
//! The client (`hdbconnect_async` 0.32) has no `REAL_VECTOR` type and
//! asks for data format version 8, older than the type. A server that
//! answers such a client in the old protocol sends a vector as
//! `VARBINARY`, its binary form (the dimension as a little-endian 32-bit
//! integer, then the float32 values): results take the columns the catalog
//! says are `REAL_VECTOR` and decode them ([`decode`]). A server that sends
//! the vector type itself makes the client fail ("Illegal type code 96"),
//! which [`explain`] turns into what to do. Transfers read vectors as text
//! (`TO_NVARCHAR`) and write them with `TO_REAL_VECTOR`.
//! The catalog (`SYS.TABLE_COLUMNS.LENGTH`) gives the dimension, and
//! `SYS.VECTOR_INDEXES` the indexes' settings. Without a HANA Cloud
//! server at hand these follow SAP's SQL reference and are covered by unit
//! tests only.

use dbine_driver::vector::{self, metric, param, VectorIndex};
use std::collections::BTreeMap;

/// The type's name in the catalog.
pub const TYPE: &str = "REAL_VECTOR";

/// `IndexDef::kind` of a vector index (`CREATE HNSW VECTOR INDEX`).
pub const KIND: &str = "HNSW VECTOR";

/// The engine's name for the search-time candidate list (`efSearch`), in
/// `VectorIndex::params`.
pub const EF_SEARCH: &str = "ef_search";

/// `REAL_VECTOR(n)`, or `REAL_VECTOR` (any dimension).
pub(crate) fn type_name(dims: Option<i64>) -> String {
    match dims {
        Some(n) if n > 0 => format!("{TYPE}({n})"),
        _ => TYPE.to_string(),
    }
}

/// Whether a catalog type is `REAL_VECTOR`.
pub(crate) fn is_vector(data_type: &str) -> bool {
    data_type
        .trim()
        .get(..TYPE.len())
        .is_some_and(|h| h.eq_ignore_ascii_case(TYPE))
}

/// A vector's binary form (`fvecs`: the dimension as a little-endian u32,
/// then the float32 values) as its JSON-array text; `None` when the bytes
/// aren't one.
pub(crate) fn decode(b: &[u8]) -> Option<String> {
    let (head, body) = b.split_at_checked(4)?;
    let n = u32::from_le_bytes(head.try_into().ok()?) as usize;
    if n == 0 || body.len() != n.checked_mul(4)? {
        return None;
    }
    Some(vector::text(vector::from_f32_le(body)?))
}

/// The catalog query of a table's `REAL_VECTOR` columns and dimensions.
pub(crate) fn columns_sql(schema: &str, table: &str) -> String {
    let s = |x: &str| format!("'{}'", x.replace('\'', "''"));
    format!(
        "SELECT COLUMN_NAME, TO_NVARCHAR(LENGTH) FROM SYS.TABLE_COLUMNS \
         WHERE SCHEMA_NAME = {} AND TABLE_NAME = {} AND DATA_TYPE_NAME = '{TYPE}'",
        s(schema),
        s(table)
    )
}

/// Whether a result column of this type may hold a vector sent the old
/// way (see the module's docs).
pub(crate) fn may_be_vector(type_id: hdbconnect_async::TypeId) -> bool {
    use hdbconnect_async::TypeId as T;
    matches!(type_id, T::VARBINARY | T::BINARY | T::BSTRING | T::BLOB)
}

/// The select-list item that reads a vector column as its text.
pub(crate) fn read_expr(quoted: &str) -> String {
    format!("TO_NVARCHAR({quoted}) AS {quoted}")
}

/// HANA's text of a vector (`[1,2.5]`, `[1.0e0,…]`) in the canonical form.
pub(crate) fn canonical(text: &str) -> String {
    vector::parse(text)
        .map(|v| vector::text(v.into_iter().map(|x| x as f32)))
        .unwrap_or_else(|| text.to_string())
}

/// A vector edit's value as a literal: `TO_REAL_VECTOR('[1,2]')`.
pub(crate) fn literal(v: &serde_json::Value) -> Option<String> {
    vector::value_text(v).map(|t| format!("TO_REAL_VECTOR('{t}')"))
}

/// Whether a result's or a describe's column holds vectors: its vector
/// meta, or a `REAL_VECTOR` type.
pub(crate) fn is_column(c: &dbine_driver::ResultColumn) -> bool {
    c.vector.is_some()
        || c.type_name
            .trim()
            .get(..TYPE.len())
            .is_some_and(|t| t.eq_ignore_ascii_case(TYPE))
}

/// What to do when the client can't decode a `REAL_VECTOR` result (a
/// server that sends the type itself to this client).
pub(crate) fn explain(message: &str) -> Option<&'static str> {
    message.contains("Illegal type code 96").then_some(
        "El resultado tiene una columna REAL_VECTOR y el cliente de HANA que usa DBine no lee ese tipo: \
         pedila como texto con TO_NVARCHAR(columna).",
    )
}

/// A browse query's table (`SELECT *\nFROM "S"."T"`, then maybe the grid's
/// filter, sort and the limit): (schema, table, the query after `SELECT
/// *`). `None` for any other query.
pub(crate) fn browsed_table(sql: &str) -> Option<(String, String, &str)> {
    let rest = sql.strip_prefix("SELECT *")?;
    let mut names = rest.strip_prefix("\nFROM ")?;
    let mut parts = Vec::new();
    loop {
        let body = names.strip_prefix('"')?;
        let mut name = String::new();
        let mut chars = body.char_indices();
        let end = loop {
            match chars.next()? {
                (i, '"') if body[i + 1..].starts_with('"') => {
                    name.push('"');
                    chars.next();
                }
                (i, '"') => break i + 1,
                (_, c) => name.push(c),
            }
        };
        parts.push(name);
        names = &body[end..];
        match names.strip_prefix('.') {
            Some(r) if parts.len() < 2 => names = r,
            Some(_) => return None,
            None => break,
        }
    }
    if !(names.is_empty() || names.starts_with(char::is_whitespace)) {
        return None;
    }
    let mut parts = parts.into_iter();
    match (parts.next(), parts.next()) {
        (Some(s), Some(t)) => Some((s, t, rest)),
        _ => None,
    }
}

/// A table's columns in order: name, type and length.
pub(crate) fn all_columns_sql(schema: &str, table: &str) -> String {
    let s = |x: &str| format!("'{}'", x.replace('\'', "''"));
    format!(
        "SELECT COLUMN_NAME, DATA_TYPE_NAME, TO_NVARCHAR(LENGTH) FROM SYS.TABLE_COLUMNS \
         WHERE SCHEMA_NAME = {} AND TABLE_NAME = {} ORDER BY POSITION",
        s(schema),
        s(table)
    )
}

/// Whether a statement declares a vector column or makes a vector index.
pub(crate) fn mentions_vectors(statement: &str) -> bool {
    let up = statement.to_ascii_uppercase();
    up.contains(TYPE) || up.contains("VECTOR INDEX")
}

/// A statement with a vector type or index that a server other than HANA
/// Cloud refused (`version`: `SYS.M_DATABASE.VERSION`, HANA Cloud's start
/// with 4; empty unknown, no hint): the error says where they are, since
/// the designer offers them on every server.
pub(crate) fn with_release_hint(e: dbine_driver::Error, version: &str) -> dbine_driver::Error {
    let v = version.trim();
    match e {
        dbine_driver::Error::Statement(mut se) if !v.is_empty() && !v.starts_with('4') => {
            se.message.push_str(&format!(
                " — SAP HANA {v} no tiene el tipo REAL_VECTOR ni índices vectoriales: son de SAP HANA Cloud."
            ));
            dbine_driver::Error::Statement(se)
        }
        other => other,
    }
}

/// HANA's similarity functions and the standard distances.
pub(crate) fn metric_of(native: &str) -> String {
    match native.trim().to_ascii_uppercase().as_str() {
        "COSINE_SIMILARITY" | "COSINE" => metric::COSINE.into(),
        "L2DISTANCE" | "L2" | "EUCLIDEAN" => metric::L2.into(),
        other => other.to_ascii_lowercase(),
    }
}

/// The `SIMILARITY FUNCTION …` of a standard (or HANA) distance.
pub(crate) fn native_metric(m: &str) -> Option<&'static str> {
    match m.trim().to_ascii_lowercase().as_str() {
        metric::COSINE | "cosine_similarity" => Some("COSINE_SIMILARITY"),
        metric::L2 | "l2distance" | "euclidean" => Some("L2DISTANCE"),
        _ => None,
    }
}

/// A `SYS.VECTOR_INDEXES` row (column name → value) as the index's
/// [`VectorIndex`]: its similarity function and the build / search
/// configurations' `M`, `efConstruction` and `efSearch`.
pub(crate) fn from_catalog(row: &BTreeMap<String, String>) -> VectorIndex {
    let find = |needle: &str| {
        row.iter()
            .find(|(k, _)| k.to_ascii_uppercase().contains(needle))
            .map(|(_, v)| v.as_str())
    };
    let mut params = BTreeMap::new();
    for (config, keys) in [
        (
            "BUILD",
            &[("M", param::M), ("efConstruction", param::EF_CONSTRUCTION)][..],
        ),
        ("SEARCH", &[("efSearch", EF_SEARCH)][..]),
    ] {
        let Some(json) = find(config) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        for (native, standard) in keys {
            if let Some(n) = v.get(native).and_then(|n| n.as_i64()) {
                params.insert(standard.to_string(), n.to_string());
            }
        }
    }
    VectorIndex {
        algorithm: Some("hnsw".into()),
        metric: find("SIMILARITY").map(metric_of),
        params,
    }
}

/// `CREATE HNSW VECTOR INDEX … ON t (c) SIMILARITY FUNCTION … BUILD
/// CONFIGURATION '{…}' SEARCH CONFIGURATION '{…}'`, or why it can't be.
pub(crate) fn index_sql(
    table: &str,
    name: &str,
    column: Option<&str>,
    v: &VectorIndex,
) -> Result<String, String> {
    let column = column.ok_or_else(|| "el índice vectorial no tiene columna".to_string())?;
    if let Some(a) = v
        .algorithm
        .as_deref()
        .filter(|a| !a.eq_ignore_ascii_case("hnsw"))
    {
        return Err(format!("HANA no tiene el algoritmo «{a}» (solo HNSW)"));
    }
    let mut s = format!("CREATE HNSW VECTOR INDEX {name} ON {table} ({column})");
    if let Some(m) = v.metric.as_deref() {
        let f = native_metric(m).ok_or_else(|| {
            format!("HANA no tiene la distancia «{m}» (solo coseno y euclidiana)")
        })?;
        s.push_str(&format!(" SIMILARITY FUNCTION {f}"));
    }
    let number = |k: &str| v.params.get(k).and_then(|x| x.trim().parse::<u64>().ok());
    let mut build = serde_json::Map::new();
    if let Some(m) = number(param::M) {
        build.insert("M".into(), m.into());
    }
    if let Some(ef) = number(param::EF_CONSTRUCTION) {
        build.insert("efConstruction".into(), ef.into());
    }
    if !build.is_empty() {
        s.push_str(&format!(
            " BUILD CONFIGURATION '{}'",
            serde_json::Value::Object(build)
        ));
    }
    if let Some(ef) = number(EF_SEARCH) {
        s.push_str(&format!(" SEARCH CONFIGURATION '{{\"efSearch\":{ef}}}'"));
    }
    Ok(s)
}

/// What the designer offers: HNSW, cosine or euclidean, `M`,
/// `efConstruction` and `efSearch`.
pub(crate) fn spec() -> dbine_driver::VectorIndexSpec {
    use dbine_driver::VectorParamSpec as P;
    dbine_driver::VectorIndexSpec {
        algorithms: vec!["hnsw"],
        metrics: vec![metric::COSINE, metric::L2],
        params: vec![
            P {
                key: param::M,
                algorithms: vec![],
                default: "64",
            },
            P {
                key: param::EF_CONSTRUCTION,
                algorithms: vec![],
                default: "128",
            },
            P {
                key: EF_SEARCH,
                algorithms: vec![],
                default: "",
            },
        ],
        filter_columns: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn types_and_text() {
        assert_eq!(type_name(Some(1536)), "REAL_VECTOR(1536)");
        assert_eq!(type_name(Some(0)), "REAL_VECTOR");
        assert_eq!(type_name(None), "REAL_VECTOR");
        assert!(is_vector("REAL_VECTOR(3)") && !is_vector("REAL"));
        assert_eq!(read_expr("\"V\""), "TO_NVARCHAR(\"V\") AS \"V\"");
        assert_eq!(canonical("[1.0e0,2.5,3.0000001e-01]"), "[1,2.5,0.3]");
        assert_eq!(canonical("not a vector"), "not a vector");
        assert_eq!(
            literal(&json!([1.0, 2.5])).as_deref(),
            Some("TO_REAL_VECTOR('[1,2.5]')")
        );
        assert!(explain("impl error: Illegal type code 96").is_some());
        assert!(explain("something else").is_none());
    }

    #[test]
    fn browse_queries_name_their_table() {
        use dbine_driver::sql::{select_top, Limit, Quote};
        let b = select_top(Quote::Double, Limit::Limit, Some("APP"), "DOCS", 200);
        let (s, t, rest) = browsed_table(&b).unwrap();
        assert_eq!((s.as_str(), t.as_str()), ("APP", "DOCS"));
        assert_eq!(rest, "\nFROM \"APP\".\"DOCS\"\nLIMIT 200");
        let odd = select_top(Quote::Double, Limit::Limit, Some("a\"b"), "c.d", 5);
        let (s, t, _) = browsed_table(&odd).unwrap();
        assert_eq!((s.as_str(), t.as_str()), ("a\"b", "c.d"));
        assert!(browsed_table("SELECT * FROM T").is_none());
        assert!(browsed_table("SELECT *\nFROM \"T\"").is_none(), "no schema");
        assert!(browsed_table("SELECT *\nFROM \"A\".\"B\".\"C\"").is_none());
        assert!(browsed_table("SELECT a\nFROM \"A\".\"B\"").is_none());
        assert_eq!(
            all_columns_sql("A", "B"),
            "SELECT COLUMN_NAME, DATA_TYPE_NAME, TO_NVARCHAR(LENGTH) FROM SYS.TABLE_COLUMNS \
             WHERE SCHEMA_NAME = 'A' AND TABLE_NAME = 'B' ORDER BY POSITION"
        );
    }

    #[test]
    fn other_servers_say_where_vectors_are() {
        let e = || {
            dbine_driver::Error::Statement(Box::new(dbine_driver::ScriptError::new(
                "[257] sql syntax error",
            )))
        };
        let msg = |e: dbine_driver::Error| match e {
            dbine_driver::Error::Statement(se) => se.message,
            other => other.to_string(),
        };
        assert!(mentions_vectors("CREATE TABLE T (V REAL_VECTOR(3))"));
        assert!(mentions_vectors("create hnsw vector index ix on t (v)"));
        assert!(!mentions_vectors("CREATE TABLE T (V INT)"));
        assert!(
            msg(with_release_hint(e(), "2.00.076.00.1705400033")).contains("son de SAP HANA Cloud")
        );
        assert_eq!(
            msg(with_release_hint(e(), "4.00.000.00.1729232516")),
            "[257] sql syntax error"
        );
        assert_eq!(msg(with_release_hint(e(), "")), "[257] sql syntax error");
    }

    #[test]
    fn binary_vectors_decode() {
        let mut b = 3u32.to_le_bytes().to_vec();
        for x in [1.0f32, 2.5, -0.1] {
            b.extend_from_slice(&x.to_le_bytes());
        }
        assert_eq!(decode(&b).as_deref(), Some("[1,2.5,-0.1]"));
        assert_eq!(decode(&b[..b.len() - 1]), None, "short");
        assert_eq!(decode(&[0, 0, 0, 0]), None, "no dimensions");
        assert_eq!(decode(&[1, 2]), None);
        assert_eq!(
            columns_sql("A'S", "T"),
            "SELECT COLUMN_NAME, TO_NVARCHAR(LENGTH) FROM SYS.TABLE_COLUMNS \
             WHERE SCHEMA_NAME = 'A''S' AND TABLE_NAME = 'T' AND DATA_TYPE_NAME = 'REAL_VECTOR'"
        );
        assert!(may_be_vector(hdbconnect_async::TypeId::VARBINARY));
        assert!(!may_be_vector(hdbconnect_async::TypeId::NVARCHAR));
    }

    #[test]
    fn indexes_both_ways() {
        let row: BTreeMap<String, String> = [
            ("INDEX_NAME", "IX"),
            ("SIMILARITY_FUNCTION", "L2DISTANCE"),
            ("BUILD_CONFIGURATION", r#"{"M":32,"efConstruction":200}"#),
            ("SEARCH_CONFIGURATION", r#"{"efSearch":50}"#),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let v = from_catalog(&row);
        assert_eq!(v.metric.as_deref(), Some("l2"));
        assert_eq!(v.params.get("m").map(String::as_str), Some("32"));
        assert_eq!(v.params.get("ef_search").map(String::as_str), Some("50"));
        assert_eq!(
            index_sql("\"T\"", "\"IX\"", Some("\"V\""), &v).unwrap(),
            "CREATE HNSW VECTOR INDEX \"IX\" ON \"T\" (\"V\") SIMILARITY FUNCTION L2DISTANCE \
             BUILD CONFIGURATION '{\"M\":32,\"efConstruction\":200}' SEARCH CONFIGURATION '{\"efSearch\":50}'"
        );
        let plain = VectorIndex {
            metric: Some("cosine".into()),
            ..Default::default()
        };
        assert_eq!(
            index_sql("t", "ix", Some("v"), &plain).unwrap(),
            "CREATE HNSW VECTOR INDEX ix ON t (v) SIMILARITY FUNCTION COSINE_SIMILARITY"
        );
        let ip = VectorIndex {
            metric: Some("ip".into()),
            ..Default::default()
        };
        assert!(index_sql("t", "ix", Some("v"), &ip).is_err());
        let ivf = VectorIndex {
            algorithm: Some("ivf".into()),
            ..Default::default()
        };
        assert!(index_sql("t", "ix", Some("v"), &ivf).is_err());
        assert!(index_sql("t", "ix", None, &plain).is_err());
    }
}
