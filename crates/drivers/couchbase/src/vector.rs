//! Vectors (embeddings) in Couchbase Server 8.0+ GSI vector indexes.
//!
//! - **Hyperscale** (`CREATE VECTOR INDEX i ON ks(emb VECTOR) INCLUDE (…)
//!   WITH {…}`, kind `HYPERSCALE`): one vector key; scalar fields go in
//!   `INCLUDE`.
//! - **Composite** (`CREATE INDEX i ON ks(cat, emb VECTOR) WITH {…}`, kind
//!   `COMPOSITE`): one vector key among scalar keys; where it goes among
//!   them is the option `vector_position` (0 = first) when it isn't first.
//!
//! Both take in `WITH` the `dimension`, the `similarity` (`COSINE`, `L2` /
//! `EUCLIDEAN`, `L2_SQUARED` / `EUCLIDEAN_SQUARED`, `DOT`) and the
//! `description` (`IVF[<lists>][,<quantization>]`, like `IVF,SQ8` or
//! `IVF1024,PQ32x8`). In the schema model the vector field is
//! `columns[0]` (its name, as the field), the other keys or the `INCLUDE`
//! fields follow; [`VectorIndex`] has the metric (`cosine`, `l2`, `ip`,
//! or the engine's `l2_squared`), and the parameters `lists` and
//! `quantization`. The dimension is the field's type, `vector(N)`, when it
//! is a field of the collection, otherwise the `dimension` option.
//!
//! Search (FTS) vector indexes live in the Search service, outside SQL++:
//! the explorer lists them ([`fts_usages`]); they aren't scripted.

use dbine_driver::vector::{metric, param};
use dbine_driver::{
    ColumnDef, Error, IndexDef, IndexUsage, Result, TableSchema, VectorElement, VectorIndex,
    VectorIndexSpec, VectorMeta, VectorParamSpec,
};
use serde_json::Value;

pub const HYPERSCALE: &str = "HYPERSCALE";
pub const COMPOSITE: &str = "COMPOSITE";
/// Option: the vector key's place among a composite index's keys.
pub const POSITION: &str = "vector_position";
/// Option: the dimension, when the field isn't a column.
pub const DIMENSION: &str = "dimension";

/// `` `emb` VECTOR `` → `emb` (`` `a`.`b` VECTOR `` → `a.b`); `None` for a
/// key that isn't a vector one.
pub fn vector_key(key: &str) -> Option<String> {
    let k = key.trim();
    let upper = k.to_ascii_uppercase();
    let field = upper
        .strip_suffix(" VECTOR")
        .map(|_| k[..k.len() - " VECTOR".len()].trim())?;
    Some(
        field
            .split('.')
            .map(|p| p.trim().trim_matches('`').replace("``", "`"))
            .collect::<Vec<_>>()
            .join("."),
    )
}

/// A field type `vector(N)`: its dimension (`Some(None)`: no dimension).
pub fn parse_type(t: &str) -> Option<Option<u32>> {
    t.split('|').find_map(|t| {
        let lower = t.trim().to_ascii_lowercase();
        let rest = lower.strip_prefix("vector")?.trim();
        if rest.is_empty() {
            return Some(None);
        }
        let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
        let mut dims = None;
        for p in inner.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match p.parse::<u32>() {
                Ok(n) => dims = Some(n),
                Err(_) if matches!(p, "float32" | "float") => {}
                Err(_) => return None,
            }
        }
        Some(dims)
    })
}

pub fn type_name(dims: Option<u32>) -> String {
    match dims {
        Some(n) => format!("vector({n})"),
        None => "vector".into(),
    }
}

fn metric_of(similarity: &str) -> String {
    match similarity.to_ascii_lowercase().as_str() {
        "cosine" => metric::COSINE.into(),
        "l2" | "euclidean" => metric::L2.into(),
        "dot" => metric::IP.into(),
        "l2_squared" | "euclidean_squared" => "l2_squared".into(),
        other => other.into(),
    }
}

fn similarity_of(m: &str) -> Result<&'static str> {
    Ok(match m.to_ascii_lowercase().as_str() {
        metric::COSINE => "COSINE",
        metric::L2 | "euclidean" => "L2",
        metric::IP | "dot" => "DOT",
        "l2_squared" | "euclidean_squared" => "L2_SQUARED",
        other => {
            return Err(Error::Query(format!(
                "Couchbase no tiene la distancia «{other}» (cosine, l2, l2_squared o dot)."
            )))
        }
    })
}

/// `IVF1024,SQ8` → (`Some("1024")`, `Some("SQ8")`).
fn parse_description(d: &str) -> (Option<String>, Option<String>) {
    let mut parts = d.split(',').map(str::trim);
    let lists = parts
        .next()
        .and_then(|p| p.strip_prefix("IVF").or_else(|| p.strip_prefix("ivf")))
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    let quant = parts.next().filter(|q| !q.is_empty()).map(str::to_string);
    (lists, quant)
}

/// The fields of an `INCLUDE (…)` clause in an index's definition.
fn include_of(definition: &str) -> Vec<String> {
    let upper = definition.to_ascii_uppercase();
    let Some(at) = upper.find(" INCLUDE") else {
        return Vec::new();
    };
    let rest = &definition[at + " INCLUDE".len()..];
    let Some(open) = rest.find('(') else {
        return Vec::new();
    };
    let mut depth = 0;
    let mut end = None;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return Vec::new();
    };
    rest[open + 1..end]
        .split(',')
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

/// Settings of `WITH` that are the vector index's own (or its default):
/// they aren't options.
fn vector_with(k: &str, v: &Value) -> bool {
    // `defer_build` is how it was built (see `ddl::table_ddl`), not what
    // it is.
    matches!(
        k,
        "dimension" | "similarity" | "description" | "defer_build"
    ) || (k == "scan_nprobes" && v.as_i64() == Some(1))
}

/// A `system:indexes` row's vector index, filled into `ix` (already read
/// as an ordinary GSI index). `false` when the row isn't a vector index.
pub fn from_row(r: &Value, ix: &mut IndexDef) -> bool {
    let Some(pos) = ix.columns.iter().position(|c| vector_key(c).is_some()) else {
        return false;
    };
    let field = vector_key(&ix.columns[pos]).unwrap_or_default();
    let definition = r
        .get("definition")
        .or_else(|| r.get("metadata").and_then(|m| m.get("definition")))
        .and_then(Value::as_str)
        .unwrap_or("");
    let hyperscale = definition
        .trim_start()
        .to_ascii_uppercase()
        .starts_with("CREATE VECTOR INDEX");
    let mut keys = ix.columns.clone();
    keys.remove(pos);
    let mut columns = vec![field];
    columns.extend(keys);
    if hyperscale {
        columns.extend(include_of(definition));
    } else if pos > 0 {
        ix.options.insert(POSITION.into(), pos.to_string());
    }
    let with = r.get("with").and_then(Value::as_object);
    let mut v = VectorIndex {
        metric: with
            .and_then(|w| w.get("similarity"))
            .and_then(Value::as_str)
            .map(metric_of),
        ..Default::default()
    };
    if let Some(d) = with
        .and_then(|w| w.get("description"))
        .and_then(Value::as_str)
    {
        let (lists, quant) = parse_description(d);
        if let Some(l) = lists {
            v.params.insert(param::LISTS.into(), l);
        }
        if let Some(q) = quant {
            v.params.insert(param::QUANTIZATION.into(), q);
        }
    }
    for (k, x) in with.into_iter().flatten() {
        if vector_with(k, x) {
            ix.options.remove(k);
        }
    }
    if let Some(n) = with
        .and_then(|w| w.get("dimension"))
        .and_then(Value::as_u64)
    {
        ix.options.insert(DIMENSION.into(), n.to_string());
    }
    ix.columns = columns;
    ix.kind = Some(if hyperscale { HYPERSCALE } else { COMPOSITE }.into());
    ix.vector = Some(v);
    true
}

/// A vector index: kind `HYPERSCALE` / `COMPOSITE`, or a designer row with
/// "Vectorial" on.
pub fn is_vector(ix: &IndexDef) -> bool {
    ix.vector.is_some()
        || ix.kind.as_deref().is_some_and(|k| {
            k.eq_ignore_ascii_case(HYPERSCALE) || k.eq_ignore_ascii_case(COMPOSITE)
        })
}

/// `ix` with its dimension from the vector field's type in `t` (when the
/// field is a column of type `vector(N)`).
pub fn with_dims(t: &TableSchema, ix: &IndexDef) -> IndexDef {
    let mut out = ix.clone();
    if !is_vector(ix) {
        return out;
    }
    if let Some(n) = ix
        .columns
        .first()
        .and_then(|f| t.columns.iter().find(|c| c.name == *f))
        .and_then(|c| parse_type(&c.data_type))
        .flatten()
    {
        out.options.insert(DIMENSION.into(), n.to_string());
    }
    out
}

/// The read-back collection: each vector index's field gets its
/// `vector(N)` type (added when no sampled document has it), and the
/// dimension leaves the index's options.
pub fn describe(t: &mut TableSchema) {
    for ix in t.indexes.iter_mut().filter(|i| i.vector.is_some()) {
        let Some(field) = ix.columns.first().cloned() else {
            continue;
        };
        let dims = ix.options.get(DIMENSION).and_then(|n| n.parse().ok());
        match t.columns.iter_mut().find(|c| c.name == field) {
            Some(c) => c.data_type = type_name(dims),
            None => t.columns.push(ColumnDef {
                name: field,
                data_type: type_name(dims),
                nullable: true,
                ..Default::default()
            }),
        }
        if dims.is_some() {
            ix.options.remove(DIMENSION);
        }
    }
}

/// The statement of a vector index (see the module docs). `keyspace` is
/// the collection's path; `field` writes a key as an index takes it.
pub fn ddl(
    keyspace: &str,
    ix: &IndexDef,
    if_not_exists: bool,
    field: impl Fn(&str) -> String,
) -> Result<String> {
    let Some(vector_field) = ix.columns.first().filter(|c| !c.trim().is_empty()) else {
        return Err(Error::Query(format!(
            "El índice {} no tiene campos.",
            ix.name
        )));
    };
    let dims: u32 = ix
        .options
        .get(DIMENSION)
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| {
            Error::Query(format!(
                "El índice vectorial {} necesita la dimensión: el campo «{vector_field}» tiene que ser de tipo vector(N).",
                ix.name
            ))
        })?;
    let v = ix.vector.clone().unwrap_or_default();
    let similarity = similarity_of(v.metric.as_deref().unwrap_or(metric::COSINE))?;
    let lists = v.params.get(param::LISTS).map(|s| s.trim()).unwrap_or("");
    let quant = v
        .params
        .get(param::QUANTIZATION)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("SQ8");
    for k in v.params.keys() {
        if k != param::LISTS && k != param::QUANTIZATION {
            return Err(Error::Query(format!(
                "El índice vectorial {} no toma el parámetro «{k}» (lists y quantization; los demás van como opciones de WITH).",
                ix.name
            )));
        }
    }
    let hyperscale = !ix
        .kind
        .as_deref()
        .is_some_and(|k| k.eq_ignore_ascii_case(COMPOSITE));
    let vkey = format!("{} VECTOR", field(vector_field));
    let rest: Vec<String> = ix.columns[1..].iter().map(|c| field(c)).collect();
    let name = if ix.name.trim().is_empty() {
        String::new()
    } else {
        format!(" {}", crate::ddl::q(&ix.name))
    };
    let ine = if if_not_exists { " IF NOT EXISTS" } else { "" };
    let mut s = if hyperscale {
        let mut s = format!("CREATE VECTOR INDEX{name}{ine} ON {keyspace}({vkey})");
        if !rest.is_empty() {
            s.push_str(&format!(" INCLUDE ({})", rest.join(", ")));
        }
        s
    } else {
        let at = ix
            .options
            .get(POSITION)
            .and_then(|p| p.parse::<usize>().ok())
            .unwrap_or(0)
            .min(rest.len());
        let mut keys = rest;
        keys.insert(at, vkey);
        format!("CREATE INDEX{name}{ine} ON {keyspace}({})", keys.join(", "))
    };
    if let Some(p) = ix.options.get("partition").filter(|p| !p.trim().is_empty()) {
        s.push_str(&format!(" PARTITION BY {p}"));
    }
    if let Some(w) = ix.filter.as_deref().filter(|w| !w.trim().is_empty()) {
        s.push_str(&format!(" WHERE {w}"));
    }
    let mut with = vec![
        format!("\"dimension\": {dims}"),
        format!("\"similarity\": \"{similarity}\""),
        format!("\"description\": \"IVF{lists},{quant}\""),
    ];
    for (k, x) in &ix.options {
        if matches!(k.as_str(), "partition" | POSITION | DIMENSION) {
            continue;
        }
        with.push(format!(
            "{}: {}",
            Value::String(k.clone()),
            serde_json::from_str::<Value>(x).unwrap_or_else(|_| Value::String(x.clone()))
        ));
    }
    s.push_str(&format!(" WITH {{{}}};", with.join(", ")));
    Ok(s)
}

/// The designer's vector index choices.
pub fn designer_spec() -> VectorIndexSpec {
    let p = |key, default| VectorParamSpec {
        key,
        algorithms: Vec::new(),
        default,
    };
    VectorIndexSpec {
        algorithms: Vec::new(),
        metrics: vec![metric::COSINE, metric::L2, "l2_squared", metric::IP],
        params: vec![p(param::QUANTIZATION, "SQ8"), p(param::LISTS, "")],
        filter_columns: true,
    }
}

/// Vector fields the bucket's vector indexes declare: field → vector.
pub fn fields(rows: &[Value]) -> std::collections::HashMap<String, VectorMeta> {
    let mut out = std::collections::HashMap::new();
    for r in rows {
        let keys: Vec<String> = r
            .get("index_key")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(vector_key)
            .collect();
        let dims = r
            .get("with")
            .and_then(|w| w.get("dimension"))
            .and_then(Value::as_u64)
            .map(|n| n as u32);
        for k in keys {
            out.insert(k, VectorMeta::dense(dims, VectorElement::F32));
        }
    }
    out
}

/// The Search service's vector indexes (`GET /api/index`, through the
/// cluster manager's `/_p/fts`) of one collection, for the explorer:
/// kind `SEARCH VECTOR`, each vector field with its dimension and
/// similarity. `bucket`, `scope` and `collection` pick the index's source
/// and its type mapping (`scope.collection`).
pub fn fts_usages(body: &Value, bucket: &str, scope: &str, collection: &str) -> Vec<IndexUsage> {
    let defs = body
        .get("indexDefs")
        .and_then(|d| d.get("indexDefs"))
        .and_then(Value::as_object);
    let mut out = Vec::new();
    for (name, def) in defs.into_iter().flatten() {
        if def.get("sourceName").and_then(Value::as_str) != Some(bucket) {
            continue;
        }
        let mapping = &def["params"]["mapping"];
        let mut fields = Vec::new();
        let types = mapping["types"].as_object();
        for (tname, t) in types.into_iter().flatten() {
            let ours = tname == &format!("{scope}.{collection}")
                || (scope == "_default" && collection == "_default" && !tname.contains('.'));
            if ours && t["enabled"].as_bool() != Some(false) {
                walk(&t["properties"], "", &mut fields);
            }
        }
        if mapping["default_mapping"]["enabled"].as_bool() == Some(true)
            && scope == "_default"
            && collection == "_default"
        {
            walk(&mapping["default_mapping"]["properties"], "", &mut fields);
        }
        if !fields.is_empty() {
            out.push(IndexUsage {
                name: name.clone(),
                kind: "SEARCH VECTOR".into(),
                key_columns: fields,
                ..Default::default()
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Vector fields of a Search mapping's `properties`: `path (vector N,
/// similarity)`.
fn walk(props: &Value, prefix: &str, out: &mut Vec<String>) {
    for (k, p) in props.as_object().into_iter().flatten() {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        for f in p["fields"].as_array().into_iter().flatten() {
            if matches!(f["type"].as_str(), Some("vector" | "vector_base64")) {
                out.push(format!(
                    "{path} (vector {}, {})",
                    f["dims"]
                        .as_u64()
                        .map(|n| n.to_string())
                        .unwrap_or("?".into()),
                    f["similarity"].as_str().unwrap_or("?")
                ));
            }
        }
        walk(&p["properties"], &path, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// As `ddl::field_path`: a key the server wrote stays as it is.
    fn q(f: &str) -> String {
        if f.contains('`') {
            return f.to_string();
        }
        f.split('.')
            .map(|p| format!("`{p}`"))
            .collect::<Vec<_>>()
            .join(".")
    }

    fn ix(row: &Value) -> IndexDef {
        let mut ix = crate::ddl::index_from_row(row);
        assert!(from_row(row, &mut ix) || ix.vector.is_some());
        ix
    }

    #[test]
    fn hyperscale_rows_read_and_write_back() {
        let row = json!({
            "name": "hv2", "index_key": ["`emb` VECTOR"],
            "partition": "HASH((meta().`id`))", "condition": "(`cat` = \"a\")",
            "with": { "description": "IVF1,SQ8", "dimension": 3, "num_partition": 2, "num_replica": 0,
                      "retain_deleted_xattr": false, "scan_nprobes": 1, "similarity": "dot" },
            "definition": "CREATE VECTOR INDEX `hv2` on `b`(`emb` VECTOR) INCLUDE(`cat`, `n`) PARTITION BY hash((meta().`id`)) WHERE (`cat` = \"a\") WITH {  \"num_partition\":2, \"dimension\":3, \"similarity\":\"DOT\", \"description\":\"IVF1,SQ8\" }"
        });
        let i = ix(&row);
        assert_eq!(i.kind.as_deref(), Some(HYPERSCALE));
        assert_eq!(i.columns, ["emb", "`cat`", "`n`"]);
        let v = i.vector.clone().unwrap();
        assert_eq!(v.metric.as_deref(), Some(metric::IP));
        assert_eq!(v.params.get("lists").map(String::as_str), Some("1"));
        assert_eq!(
            v.params.get("quantization").map(String::as_str),
            Some("SQ8")
        );
        assert_eq!(i.options.get(DIMENSION).map(String::as_str), Some("3"));
        assert!(!i.options.contains_key("scan_nprobes") && !i.options.contains_key("similarity"));
        assert_eq!(
            ddl("`b`", &i, false, q).unwrap(),
            "CREATE VECTOR INDEX `hv2` ON `b`(`emb` VECTOR) INCLUDE (`cat`, `n`) PARTITION BY HASH((meta().`id`)) WHERE (`cat` = \"a\") WITH {\"dimension\": 3, \"similarity\": \"DOT\", \"description\": \"IVF1,SQ8\", \"num_partition\": 2};"
        );
    }

    #[test]
    fn composite_rows_keep_the_vector_keys_place() {
        let row = json!({
            "name": "cvix", "index_key": ["`cat`", "`emb` VECTOR"],
            "with": { "description": "IVF,PQ32x8", "dimension": 1536, "num_partition": 1, "num_replica": 0,
                      "retain_deleted_xattr": false, "scan_nprobes": 4, "similarity": "l2_squared" },
            "definition": "CREATE INDEX `cvix` ON `b`(`cat`,`emb` VECTOR) WITH { }"
        });
        let i = ix(&row);
        assert_eq!(i.kind.as_deref(), Some(COMPOSITE));
        assert_eq!(i.columns, ["emb", "`cat`"]);
        assert_eq!(i.options.get(POSITION).map(String::as_str), Some("1"));
        assert_eq!(
            i.vector.as_ref().unwrap().metric.as_deref(),
            Some("l2_squared")
        );
        assert!(!i.vector.as_ref().unwrap().params.contains_key("lists"));
        assert_eq!(
            ddl("`b`", &i, true, q).unwrap(),
            "CREATE INDEX `cvix` IF NOT EXISTS ON `b`(`cat`, `emb` VECTOR) WITH {\"dimension\": 1536, \"similarity\": \"L2_SQUARED\", \"description\": \"IVF,PQ32x8\", \"scan_nprobes\": 4};"
        );
        // An ordinary index isn't one.
        let mut plain = crate::ddl::index_from_row(&json!({ "name": "a", "index_key": ["`a`"] }));
        assert!(!from_row(&json!({}), &mut plain));
        assert!(plain.vector.is_none());
    }

    #[test]
    fn dims_and_fields() {
        assert_eq!(vector_key("`emb` VECTOR").as_deref(), Some("emb"));
        assert_eq!(vector_key("`a`.`b` vector").as_deref(), Some("a.b"));
        assert_eq!(vector_key("`emb`"), None);
        assert_eq!(parse_type("vector(1536)"), Some(Some(1536)));
        assert_eq!(parse_type("array|vector"), Some(None));
        assert_eq!(parse_type("vector(3, int8)"), None);
        let mut t = TableSchema {
            name: "c".into(),
            indexes: vec![ix(&json!({ "name": "v", "index_key": ["`emb` VECTOR"],
                "with": { "dimension": 3, "similarity": "cosine", "description": "IVF,SQ8" },
                "definition": "CREATE VECTOR INDEX `v` ON `b`(`emb` VECTOR)" }))],
            ..Default::default()
        };
        describe(&mut t);
        assert_eq!(t.columns[0].data_type, "vector(3)");
        assert!(!t.indexes[0].options.contains_key(DIMENSION));
        assert!(ddl("`b`", &t.indexes[0], false, q).is_err());
        let with = with_dims(&t, &t.indexes[0]);
        assert!(ddl("`b`", &with, false, q)
            .unwrap()
            .contains("\"dimension\": 3"));
        // A designer row: hyperscale, the defaults.
        let d = IndexDef {
            name: "n".into(),
            columns: vec!["emb".into(), "cat".into()],
            vector: Some(VectorIndex::default()),
            ..Default::default()
        };
        assert_eq!(
            ddl("`b`", &with_dims(&t, &d), false, q).unwrap(),
            "CREATE VECTOR INDEX `n` ON `b`(`emb` VECTOR) INCLUDE (`cat`) WITH {\"dimension\": 3, \"similarity\": \"COSINE\", \"description\": \"IVF,SQ8\"};"
        );
        let bad = IndexDef {
            vector: Some(VectorIndex {
                metric: Some("hamming".into()),
                ..Default::default()
            }),
            ..d.clone()
        };
        assert!(ddl("`b`", &with_dims(&t, &bad), false, q).is_err());
        let rows = [json!({ "index_key": ["`cat`", "`emb` VECTOR"], "with": { "dimension": 3 } })];
        assert_eq!(
            fields(&rows).get("emb"),
            Some(&VectorMeta::dense(Some(3), VectorElement::F32))
        );
    }

    #[test]
    fn search_vector_indexes_are_listed() {
        let body = json!({ "indexDefs": { "indexDefs": {
            "vs": { "sourceName": "b", "params": { "mapping": {
                "default_mapping": { "enabled": false },
                "types": { "inv.docs": { "enabled": true, "properties": {
                    "emb": { "fields": [{ "name": "emb", "type": "vector", "dims": 3, "similarity": "dot_product" }] },
                    "meta": { "properties": { "e2": { "fields": [{ "type": "vector_base64", "dims": 8, "similarity": "l2_norm" }] } } },
                    "title": { "fields": [{ "type": "text" }] },
                } } } } } },
            "other": { "sourceName": "x", "params": {} },
            "text": { "sourceName": "b", "params": { "mapping": { "types": { "inv.docs": { "properties": {
                "title": { "fields": [{ "type": "text" }] } } } } } } },
        } } });
        let u = fts_usages(&body, "b", "inv", "docs");
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].kind, "SEARCH VECTOR");
        assert_eq!(
            u[0].key_columns,
            ["emb (vector 3, dot_product)", "meta.e2 (vector 8, l2_norm)"]
        );
        assert!(fts_usages(&body, "b", "inv", "other").is_empty());
    }
}
