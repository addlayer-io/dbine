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
//! Every key of a vector index is by its name (`cat`, `a.b`), as the vector
//! field is; an expression key stays as the server wrote it.
//!
//! On a collection without documents a vector index can't train: the
//! session creates it deferred and says so ([`deferred`]).
//!
//! **Search** (FTS) vector indexes live in the Search service, outside
//! SQL++ (kind [`SEARCH`]): read from `GET /api/index` into the indexes of
//! the collections they map ([`fts_indexes`]), with the first vector
//! field as `columns[0]`, its similarity (`dot_product` = `ip`, `l2_norm` =
//! `l2`, `cosine`) as the metric, `vector_index_optimized_for` as the
//! parameter `optimized_for` and the definition whole in the option
//! `definition`. Scripts make them with a DBine statement, `CREATE SEARCH
//! INDEX name ON bucket.scope.collection WITH {definition}` /
//! `DROP SEARCH INDEX name ON …`, which the session runs through the Search
//! REST API ([`search_command`]).

use dbine_driver::vector::{metric, param};
use dbine_driver::{
    ColumnDef, Error, IndexDef, IndexUsage, Result, TableSchema, VectorElement, VectorIndex,
    VectorIndexSpec, VectorMeta, VectorParamSpec,
};
use serde_json::{json, Map, Value};

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
        // Over 0/1 vectors (a bit vector from another engine, which the
        // schema conversion lets through only over bits) the squared
        // euclidean distance is hamming's.
        "l2_squared" | "euclidean_squared" | metric::HAMMING => "L2_SQUARED",
        other => {
            return Err(Error::Query(format!(
                "Couchbase no tiene la distancia «{other}» (cosine, l2, l2_squared o dot)."
            )))
        }
    })
}

/// What a vector index writes otherwise than its metric says (hamming,
/// which Couchbase hasn't, from a bit vector of another engine), for the
/// script.
pub fn metric_note(ix: &IndexDef) -> Option<String> {
    let m = ix.vector.as_ref()?.metric.as_deref()?;
    if !m.eq_ignore_ascii_case(metric::HAMMING) {
        return None;
    }
    Some(if is_search(ix) {
        format!(
            "{}: la distancia hamming queda l2_norm (el servicio Search no tiene hamming; sobre vectores de 0 y 1 los vecinos más cercanos son los mismos).",
            ix.name
        )
    } else {
        format!(
            "{}: la distancia hamming queda L2_SQUARED (Couchbase no tiene hamming; sobre vectores de 0 y 1 es la misma distancia).",
            ix.name
        )
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

/// A key that is a field path (`` `cat` ``, `` `a`.`b` ``, `cat`) as the
/// field's name (`cat`, `a.b`), as the vector field is; any other key (an
/// expression, a `DESC` key, a name with a dot) stays as the server wrote
/// it. The index writes a name back quoted (`ddl::field_path`).
pub fn plain_key(key: &str) -> String {
    let k = key.trim();
    let mut out = Vec::new();
    let mut rest = k;
    loop {
        let (seg, after) = if let Some(r) = rest.strip_prefix('`') {
            let mut name = String::new();
            let mut end = None;
            let mut it = r.char_indices().peekable();
            while let Some((i, c)) = it.next() {
                if c == '`' {
                    if it.peek().map(|p| p.1) == Some('`') {
                        it.next();
                        name.push('`');
                    } else {
                        end = Some(i + 1);
                        break;
                    }
                } else {
                    name.push(c);
                }
            }
            match end {
                Some(e) if !name.is_empty() && !name.contains('.') => (name, &r[e..]),
                _ => return k.to_string(),
            }
        } else {
            let end = rest.find('.').unwrap_or(rest.len());
            let word = &rest[..end];
            let ident = word
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ident {
                return k.to_string();
            }
            (word.to_string(), &rest[end..])
        };
        out.push(seg);
        match after.strip_prefix('.') {
            Some(r) => rest = r,
            None if after.is_empty() => break,
            None => return k.to_string(),
        }
    }
    out.join(".")
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
    // Every key by the same form as the vector field: a field path by its
    // name (an index on another engine reads them as field names).
    let mut columns = vec![field];
    columns.extend(keys.iter().map(|k| plain_key(k)));
    if hyperscale {
        columns.extend(include_of(definition).iter().map(|k| plain_key(k)));
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

/// A `CREATE [VECTOR] INDEX` with a vector key built at once (no
/// `defer_build`): the same statement deferred, and its keyspace. On a
/// collection without documents the session runs that instead (a vector
/// index trains on the documents), and says so.
pub fn deferred(stmt: &str) -> Option<(String, String)> {
    let s = stmt.trim().trim_end_matches(';').trim_end();
    let upper = s.to_ascii_uppercase();
    if !(upper.starts_with("CREATE VECTOR INDEX") || upper.starts_with("CREATE INDEX"))
        || upper.contains("DEFER_BUILD")
    {
        return None;
    }
    let on = upper.find(" ON ")?;
    let open = on + upper[on..].find('(')?;
    let mut depth = 0usize;
    let mut close = None;
    for (i, c) in upper[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let keys = &upper[open + 1..close];
    if !keys.split(',').any(|k| k.trim().ends_with(" VECTOR")) {
        return None;
    }
    let keyspace = s[on + 4..open].trim().to_string();
    let out = match upper[close..].find(" WITH ") {
        Some(w) => {
            let brace = close + w + upper[close + w..].find('{')?;
            let inner = s[brace + 1..].trim_start();
            let sep = if inner.starts_with('}') { "" } else { ", " };
            format!("{}\"defer_build\": true{sep}{}", &s[..brace + 1], inner)
        }
        None => format!("{s} WITH {{\"defer_build\": true}}"),
    };
    Some((format!("{out};"), keyspace))
}

/// A vector index: kind `HYPERSCALE` / `COMPOSITE`, or a designer row with
/// "Vectorial" on (a Search one too, see [`is_search`]).
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
            let ours = mapped_collection(tname) == (scope.to_string(), collection.to_string());
            if ours && t["enabled"].as_bool() != Some(false) {
                fts_fields(&t["properties"], "", &mut fields);
            }
        }
        if mapping["default_mapping"]["enabled"].as_bool() == Some(true)
            && scope == "_default"
            && collection == "_default"
        {
            fts_fields(&mapping["default_mapping"]["properties"], "", &mut fields);
        }
        if let Some(first) = fields.first() {
            out.push(IndexUsage {
                name: short_name(def["name"].as_str().unwrap_or(name), bucket),
                kind: "SEARCH VECTOR".into(),
                vector: Some(fts_vector(first)),
                key_columns: fields
                    .iter()
                    .map(|f| {
                        format!(
                            "{} (vector {}, {})",
                            f.path,
                            f.dims.map(|n| n.to_string()).unwrap_or("?".into()),
                            f.similarity.as_deref().unwrap_or("?")
                        )
                    })
                    .collect(),
                ..Default::default()
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ---------------------------------------------------------------- Search (FTS)

/// Kind of a Search (FTS) index with vector fields.
pub const SEARCH: &str = "SEARCH";

/// A Search index with vector fields (kind [`SEARCH`]).
pub fn is_search(ix: &IndexDef) -> bool {
    ix.kind
        .as_deref()
        .is_some_and(|k| k.trim().eq_ignore_ascii_case(SEARCH))
}

/// A vector field of a Search mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct FtsField {
    pub path: String,
    pub dims: Option<u64>,
    pub similarity: Option<String>,
    /// `vector_index_optimized_for` (`recall`, `latency`,
    /// `memory-efficient`).
    pub optimized_for: Option<String>,
    /// `vector_base64`: the vector is a base64 string of float32s.
    pub base64: bool,
}

/// The vector fields of a Search mapping's `properties`, nested ones
/// (`meta.e2`) included.
pub fn fts_fields(props: &Value, prefix: &str, out: &mut Vec<FtsField>) {
    for (k, p) in props.as_object().into_iter().flatten() {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        for f in p["fields"].as_array().into_iter().flatten() {
            let t = f["type"].as_str();
            if matches!(t, Some("vector" | "vector_base64")) {
                out.push(FtsField {
                    path: path.clone(),
                    dims: f["dims"].as_u64(),
                    similarity: f["similarity"].as_str().map(str::to_string),
                    optimized_for: f["vector_index_optimized_for"].as_str().map(str::to_string),
                    base64: t == Some("vector_base64"),
                });
            }
        }
        fts_fields(&p["properties"], &path, out);
    }
}

/// A Search index's vector settings, from its first vector field: the
/// similarity as the metric, `vector_index_optimized_for` as the parameter
/// `optimized_for`.
fn fts_vector(first: &FtsField) -> VectorIndex {
    let mut params = std::collections::BTreeMap::new();
    if let Some(o) = &first.optimized_for {
        params.insert("optimized_for".to_string(), o.clone());
    }
    VectorIndex {
        algorithm: None,
        metric: first.similarity.as_deref().map(fts_metric),
        params,
    }
}

/// A Search similarity as a metric, and back.
fn fts_metric(s: &str) -> String {
    match s.to_ascii_lowercase().as_str() {
        "dot_product" => metric::IP.into(),
        "l2_norm" => metric::L2.into(),
        "cosine" => metric::COSINE.into(),
        other => other.into(),
    }
}

fn fts_similarity(m: &str) -> Result<&'static str> {
    Ok(match m.to_ascii_lowercase().as_str() {
        metric::IP | "dot_product" | "dot" => "dot_product",
        metric::L2
        | "l2_norm"
        | "euclidean"
        | "l2_squared"
        | "euclidean_squared"
        | metric::HAMMING => "l2_norm",
        metric::COSINE => "cosine",
        other => {
            return Err(Error::Query(format!(
            "El servicio Search no tiene la distancia «{other}» (dot_product, l2_norm o cosine)."
        )))
        }
    })
}

/// `scope.collection[.type]` of a Search type mapping: the collection it
/// maps (a name without dots: a type of the default collection).
fn mapped_collection(type_name: &str) -> (String, String) {
    let mut parts = type_name.splitn(3, '.');
    match (parts.next(), parts.next()) {
        (Some(s), Some(c)) => (s.to_string(), c.to_string()),
        _ => ("_default".into(), "_default".into()),
    }
}

/// An index's name without the `bucket.scope.` the Search service puts
/// before a scoped index's.
fn short_name(name: &str, bucket: &str) -> String {
    name.strip_prefix(&format!("{bucket}."))
        .and_then(|r| r.split_once('.').map(|(_, n)| n.to_string()))
        .unwrap_or_else(|| name.to_string())
}

/// Option: a Search index that maps other collections too (they keep
/// their own mappings; the script makes this collection's).
pub const ALSO_MAPS: &str = "also_maps";

/// The Search service's vector indexes on `bucket` (`GET /api/index`, through
/// the cluster manager's `/_p/fts`), as indexes of the collections they
/// map: `(scope, collection, index)`. Kind [`SEARCH`], `columns[0]` the
/// first vector field, its similarity as the metric,
/// `vector_index_optimized_for` as the parameter `optimized_for`, the
/// dimension in [`DIMENSION`], and the definition (that collection's
/// mapping, without the server's ids) in the option `definition`.
pub fn fts_indexes(body: &Value, bucket: &str) -> Vec<(String, String, IndexDef)> {
    let defs = body
        .get("indexDefs")
        .and_then(|d| d.get("indexDefs"))
        .and_then(Value::as_object);
    let mut out = Vec::new();
    for (key, def) in defs.into_iter().flatten() {
        if def.get("sourceName").and_then(Value::as_str) != Some(bucket) {
            continue;
        }
        let name = short_name(def["name"].as_str().unwrap_or(key), bucket);
        let mapping = &def["params"]["mapping"];
        let mut targets: Vec<(String, String, Option<String>, Vec<FtsField>)> = Vec::new();
        for (tname, t) in mapping["types"].as_object().into_iter().flatten() {
            if t["enabled"].as_bool() == Some(false) {
                continue;
            }
            let mut fields = Vec::new();
            fts_fields(&t["properties"], "", &mut fields);
            let (s, c) = mapped_collection(tname);
            targets.push((s, c, Some(tname.clone()), fields));
        }
        if mapping["default_mapping"]["enabled"].as_bool() == Some(true) {
            let mut fields = Vec::new();
            fts_fields(&mapping["default_mapping"]["properties"], "", &mut fields);
            targets.push(("_default".into(), "_default".into(), None, fields));
        }
        let mapped: Vec<String> = targets
            .iter()
            .filter(|t| !t.3.is_empty())
            .map(|t| format!("{}.{}", t.0, t.1))
            .collect();
        for (scope, coll, tname, fields) in targets {
            let Some(first) = fields.first() else {
                continue;
            };
            let mut d = def.clone();
            if let Some(o) = d.as_object_mut() {
                for k in ["uuid", "sourceUUID", "name", "sourceName", "prevIndexUUID"] {
                    o.remove(k);
                }
            }
            // This collection's mapping only.
            if let Some(types) = d
                .pointer_mut("/params/mapping/types")
                .and_then(Value::as_object_mut)
            {
                types.retain(|k, _| Some(k) == tname.as_ref());
            }
            let mut options = std::collections::BTreeMap::new();
            if let Some(n) = first.dims {
                options.insert(DIMENSION.to_string(), n.to_string());
            }
            options.insert("definition".to_string(), canonical(&d).to_string());
            let here = format!("{scope}.{coll}");
            let others: Vec<&String> = mapped.iter().filter(|m| **m != here).collect();
            if !others.is_empty() {
                options.insert(
                    ALSO_MAPS.to_string(),
                    others
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            out.push((
                scope,
                coll,
                IndexDef {
                    name: name.clone(),
                    columns: vec![first.path.clone()],
                    kind: Some(SEARCH.into()),
                    options,
                    vector: Some(fts_vector(first)),
                    ..Default::default()
                },
            ));
        }
    }
    out.sort_by(|a, b| (&a.0, &a.1, &a.2.name).cmp(&(&b.0, &b.1, &b.2.name)));
    out
}

/// JSON with its object keys sorted (scripts and comparisons don't depend
/// on the server's order).
pub fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), canonical(&o[k])))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// The segments of a keyspace path (`` `b`.`s`.`c` ``, `b.s.c`), without
/// their backticks.
pub fn keyspace_parts(ks: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = ks.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' if quoted && chars.peek() == Some(&'`') => {
                chars.next();
                cur.push('`');
            }
            '`' => quoted = !quoted,
            '.' if !quoted => out.push(std::mem::take(&mut cur)),
            c if !quoted && c.is_whitespace() => {}
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// The `properties` object holding the field at `path` (`a.b` → `a`'s),
/// made when missing.
fn props_for<'a>(mapping: &'a mut Value, path: &str) -> &'a mut Map<String, Value> {
    let mut cur = mapping;
    let parts: Vec<&str> = path.split('.').collect();
    for p in &parts[..parts.len() - 1] {
        if !cur["properties"].is_object() {
            cur["properties"] = json!({});
        }
        let props = cur["properties"].as_object_mut().expect("an object");
        cur = props
            .entry(p.to_string())
            .or_insert_with(|| json!({ "enabled": true, "dynamic": false, "properties": {} }));
    }
    if !cur["properties"].is_object() {
        cur["properties"] = json!({});
    }
    cur["properties"].as_object_mut().expect("an object")
}

/// The `CREATE SEARCH INDEX` of a Search vector index on `keyspace` (a
/// DBine statement, which the session runs through the Search service's
/// REST API, see [`search_command`]): its definition, with its mapping
/// moved to this collection and its vector field (`columns[0]`) with the
/// index's dimension, similarity and `optimized_for`. A new index (from
/// the designer) gets a mapping of just that field.
pub fn fts_ddl(keyspace: &str, ix: &IndexDef, if_not_exists: bool) -> Result<String> {
    let path = ix
        .columns
        .first()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| Error::Query(format!("El índice {} no tiene campos.", ix.name)))?;
    let parts = keyspace_parts(keyspace);
    let (scope, coll) = match parts.as_slice() {
        [_, s, c] => (s.clone(), c.clone()),
        [_, s] => (s.clone(), "_default".to_string()),
        [c] => ("_default".into(), c.clone()),
        _ => ("_default".into(), "_default".into()),
    };
    let mut d = match ix.options.get("definition") {
        Some(t) => serde_json::from_str::<Value>(t).map_err(|e| {
            Error::Query(format!(
                "La definición del índice de búsqueda {} no es JSON: {e}",
                ix.name
            ))
        })?,
        None => json!({
            "type": "fulltext-index",
            "sourceType": "gocbcore",
            "params": {
                "doc_config": { "mode": "scope.collection.type_field", "type_field": "type" },
                "mapping": { "default_mapping": { "enabled": false, "dynamic": true }, "types": {} },
                "store": { "indexType": "scorch" }
            }
        }),
    };
    if d.pointer("/params/mapping").is_none() {
        d["params"]["mapping"] =
            json!({ "default_mapping": { "enabled": false, "dynamic": true }, "types": {} });
    }
    // The mapping goes to this collection (keeping a type after it).
    let target = format!("{scope}.{coll}");
    let default = d["params"]["mapping"]["types"]
        .as_object()
        .is_none_or(|t| t.is_empty())
        && d["params"]["mapping"]["default_mapping"]["enabled"].as_bool() == Some(true);
    let mapping: &mut Value = if default && scope == "_default" && coll == "_default" {
        &mut d["params"]["mapping"]["default_mapping"]
    } else {
        let types = &mut d["params"]["mapping"]["types"];
        if !types.is_object() {
            *types = json!({});
        }
        let t = types.as_object_mut().expect("an object");
        let (key, value) = match t.keys().next().cloned() {
            Some(k) => {
                let v = t.remove(&k).unwrap_or_default();
                let rest = k.splitn(3, '.').nth(2).map(|r| format!("{target}.{r}"));
                (rest.unwrap_or(target), v)
            }
            None => (
                target,
                json!({ "enabled": true, "dynamic": false, "properties": {} }),
            ),
        };
        t.clear();
        t.insert(key.clone(), value);
        if default {
            d["params"]["mapping"]["default_mapping"]["enabled"] = false.into();
        }
        &mut d["params"]["mapping"]["types"][key.as_str()]
    };
    // A renamed field: its mapping moves with it.
    let mut had = Vec::new();
    fts_fields(&mapping["properties"], "", &mut had);
    if let Some(old) = had.first().map(|f| f.path.clone()).filter(|o| o != path) {
        let leaf = old.rsplit('.').next().unwrap_or(&old).to_string();
        if let Some(moved) = props_for(mapping, &old).remove(&leaf) {
            let new_leaf = path.rsplit('.').next().unwrap_or(path).to_string();
            props_for(mapping, path).insert(new_leaf, moved);
        }
    }
    let leaf = path.rsplit('.').next().unwrap_or(path).to_string();
    let props = props_for(mapping, path);
    let entry = props
        .entry(leaf.clone())
        .or_insert_with(|| json!({ "enabled": true, "dynamic": false, "fields": [] }));
    if !entry["fields"].is_array() {
        entry["fields"] = json!([]);
    }
    let fields = entry["fields"].as_array_mut().expect("an array");
    let at = match fields
        .iter()
        .position(|f| matches!(f["type"].as_str(), Some("vector" | "vector_base64")))
    {
        Some(i) => i,
        None => {
            fields.push(json!({ "name": leaf, "type": "vector", "index": true }));
            fields.len() - 1
        }
    };
    let f = &mut fields[at];
    match ix
        .options
        .get(DIMENSION)
        .and_then(|n| n.trim().parse::<u32>().ok())
    {
        Some(n) => f["dims"] = n.into(),
        None if f["dims"].is_number() => {}
        None => {
            return Err(Error::Query(format!(
                "El índice vectorial {} necesita la dimensión: el campo «{path}» tiene que ser de tipo vector(N).",
                ix.name
            )))
        }
    }
    let v = ix.vector.clone().unwrap_or_default();
    match v.metric.as_deref() {
        Some(m) => f["similarity"] = fts_similarity(m)?.into(),
        None if f["similarity"].is_string() => {}
        None => f["similarity"] = "dot_product".into(),
    }
    match v
        .params
        .get("optimized_for")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(o) => f["vector_index_optimized_for"] = o.into(),
        None => {
            if let Some(o) = f.as_object_mut() {
                o.remove("vector_index_optimized_for");
            }
        }
    }
    let name = crate::ddl::q(&ix.name);
    let ine = if if_not_exists { " IF NOT EXISTS" } else { "" };
    let mut s = String::new();
    if let Some(also) = ix.options.get(ALSO_MAPS) {
        s.push_str(&format!(
            "-- El índice de búsqueda {} del original también indexa {also}: aquí se crea con la parte de esta colección.\n",
            ix.name
        ));
    }
    s.push_str(&format!(
        "CREATE SEARCH INDEX {name}{ine} ON {keyspace} WITH {};",
        canonical(&d)
    ));
    Ok(s)
}

/// `DROP SEARCH INDEX` (a DBine statement, see [`search_command`]).
pub fn fts_drop(keyspace: &str, name: &str) -> String {
    format!("DROP SEARCH INDEX {} ON {keyspace};", crate::ddl::q(name))
}

/// A DBine Search index statement, which the session runs through the
/// Search service's REST API (SQL++ doesn't make Search indexes).
#[derive(Debug, Clone, PartialEq)]
pub enum SearchCommand {
    /// `CREATE SEARCH INDEX name [IF NOT EXISTS] ON keyspace WITH {…}`.
    Create {
        name: String,
        if_not_exists: bool,
        keyspace: Vec<String>,
        definition: Value,
    },
    /// `DROP SEARCH INDEX [IF EXISTS] name ON keyspace`.
    Drop {
        name: String,
        if_exists: bool,
        keyspace: Vec<String>,
    },
}

/// The name at the start of `s` (`` `a``b` `` or a bare word) and the rest.
fn take_name(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('`') {
        let mut name = String::new();
        let mut it = rest.char_indices().peekable();
        while let Some((i, c)) = it.next() {
            if c == '`' {
                if it.peek().map(|p| p.1) == Some('`') {
                    it.next();
                    name.push('`');
                } else {
                    return Some((name, &rest[i + 1..]));
                }
            } else {
                name.push(c);
            }
        }
        None
    } else {
        let end = s
            .find(|c: char| c.is_whitespace() || c == ';')
            .unwrap_or(s.len());
        (end > 0).then(|| (s[..end].to_string(), &s[end..]))
    }
}

/// `kw` (case-insensitive words) at the start of `s`: the rest.
pub fn take_words<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let mut rest = s.trim_start();
    for w in kw.split_whitespace() {
        let head = rest.get(..w.len())?;
        if !head.eq_ignore_ascii_case(w) {
            return None;
        }
        let after = &rest[w.len()..];
        if !after.is_empty() && !after.starts_with(char::is_whitespace) {
            return None;
        }
        rest = after.trim_start();
    }
    Some(rest)
}

/// A DBine Search index statement; `None` for anything else.
pub fn search_command(stmt: &str) -> Option<Result<SearchCommand>> {
    let s = stmt.trim().trim_end_matches(';').trim_end();
    let bad = |what: &str| {
        Some(Err(Error::Query(format!(
            "{what}: CREATE SEARCH INDEX nombre ON bucket.scope.colección WITH {{definición}} o DROP SEARCH INDEX nombre ON bucket.scope.colección."
        ))))
    };
    if let Some(rest) = take_words(s, "CREATE SEARCH INDEX") {
        let Some((name, rest)) = take_name(rest) else {
            return bad("Falta el nombre del índice");
        };
        let (if_not_exists, rest) = match take_words(rest, "IF NOT EXISTS") {
            Some(r) => (true, r),
            None => (false, rest),
        };
        let Some(rest) = take_words(rest, "ON") else {
            return bad("Falta ON");
        };
        let upper = rest.to_ascii_uppercase();
        let Some(at) = upper.find(" WITH ") else {
            return bad("Falta WITH con la definición");
        };
        let keyspace = keyspace_parts(&rest[..at]);
        let definition = match serde_json::from_str::<Value>(rest[at + 6..].trim()) {
            Ok(v) if v.is_object() => v,
            _ => return bad("La definición no es un objeto JSON"),
        };
        return Some(Ok(SearchCommand::Create {
            name,
            if_not_exists,
            keyspace,
            definition,
        }));
    }
    if let Some(rest) = take_words(s, "DROP SEARCH INDEX") {
        let (if_exists, rest) = match take_words(rest, "IF EXISTS") {
            Some(r) => (true, r),
            None => (false, rest),
        };
        let Some((name, rest)) = take_name(rest) else {
            return bad("Falta el nombre del índice");
        };
        let Some(rest) = take_words(rest, "ON") else {
            return bad("Falta ON");
        };
        return Some(Ok(SearchCommand::Drop {
            name,
            if_exists,
            keyspace: keyspace_parts(rest),
        }));
    }
    None
}

/// `(bucket, scope)` of a statement's keyspace, the session's bucket and
/// `_default` filling what it leaves out.
pub fn bucket_scope(keyspace: &[String], session_bucket: Option<&str>) -> Result<(String, String)> {
    match keyspace {
        [b, s, ..] if !b.is_empty() && !s.is_empty() => Ok((b.clone(), s.clone())),
        [b] if !b.is_empty() && keyspace.len() == 1 => match session_bucket {
            // A bare collection of the session's bucket.
            Some(sb) => Ok((sb.to_string(), "_default".into())),
            None => Ok((b.clone(), "_default".into())),
        },
        _ => Err(Error::Query(
            "El índice de búsqueda necesita bucket.scope.colección.".into(),
        )),
    }
}

/// The body of a Search index `PUT`: the definition, named, on `bucket`.
pub fn fts_body(definition: &Value, name: &str, bucket: &str) -> Value {
    let mut d = definition.clone();
    d["name"] = name.into();
    d["sourceName"] = bucket.into();
    if d["type"].is_null() {
        d["type"] = "fulltext-index".into();
    }
    if d["sourceType"].is_null() {
        d["sourceType"] = "gocbcore".into();
    }
    d
}

/// The vector fields of the bucket's Search indexes, for results: field →
/// vector (array fields only: a `vector_base64` field keeps its text).
pub fn fts_result_fields(
    body: &Value,
    bucket: &str,
) -> std::collections::HashMap<String, VectorMeta> {
    let mut out = std::collections::HashMap::new();
    for (_, _, ix) in fts_indexes(body, bucket) {
        let def: Value = ix
            .options
            .get("definition")
            .and_then(|d| serde_json::from_str(d).ok())
            .unwrap_or_default();
        let mapping = &def["params"]["mapping"];
        let mut fields = Vec::new();
        for (_, t) in mapping["types"].as_object().into_iter().flatten() {
            fts_fields(&t["properties"], "", &mut fields);
        }
        fts_fields(&mapping["default_mapping"]["properties"], "", &mut fields);
        for f in fields.into_iter().filter(|f| !f.base64) {
            out.insert(
                f.path,
                VectorMeta::dense(f.dims.map(|n| n as u32), VectorElement::F32),
            );
        }
    }
    out
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
        // Every key by its name, as the vector field.
        assert_eq!(i.columns, ["emb", "cat", "n"]);
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
        assert_eq!(i.columns, ["emb", "cat"]);
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
                metric: Some("jaccard".into()),
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
        // The explorer's index has the schema's settings.
        let defs = fts_indexes(&body, "b");
        let ix = defs
            .iter()
            .find(|(s, c, _)| s == "inv" && c == "docs")
            .map(|(_, _, i)| i)
            .unwrap();
        assert_eq!(u[0].vector, ix.vector);
        assert_eq!(
            u[0].vector.as_ref().unwrap().metric.as_deref(),
            Some(metric::IP)
        );
    }

    #[test]
    fn keys_go_by_their_names() {
        assert_eq!(plain_key("`cat`"), "cat");
        assert_eq!(plain_key("`a`.`b`"), "a.b");
        assert_eq!(plain_key("cat"), "cat");
        assert_eq!(plain_key("`my-field`"), "my-field");
        assert_eq!(plain_key("`a``b`"), "a`b");
        // Expressions, DESC keys and dotted names stay as written.
        for k in [
            "lower(`name`)",
            "`cat` DESC",
            "`a.b`",
            "(meta().`id`)",
            "`a`[0]",
        ] {
            assert_eq!(plain_key(k), k, "{k}");
        }
        // A composite index written back: the same statement.
        let row = json!({
            "name": "c", "index_key": ["`my-cat`", "`emb` VECTOR", "lower(`name`)"],
            "with": { "description": "IVF,SQ8", "dimension": 2, "similarity": "cosine" },
            "definition": "CREATE INDEX `c` ON `b`(`my-cat`,`emb` VECTOR,lower(`name`))"
        });
        let i = ix(&row);
        assert_eq!(i.columns, ["emb", "my-cat", "lower(`name`)"]);
        assert_eq!(
            crate::ddl::index_ddl("`b`", &i, false).unwrap(),
            "CREATE INDEX `c` ON `b`(`my-cat`, `emb` VECTOR, lower(`name`)) WITH {\"dimension\": 2, \"similarity\": \"COSINE\", \"description\": \"IVF,SQ8\"};"
        );
    }

    #[test]
    fn an_empty_collection_gets_a_deferred_vector_index() {
        let (s, ks) = deferred(
            "CREATE VECTOR INDEX `v` ON `b`.`s`.`c`(`emb` VECTOR) INCLUDE (`cat`) WITH {\"dimension\": 3, \"similarity\": \"COSINE\"};",
        )
        .unwrap();
        assert_eq!(ks, "`b`.`s`.`c`");
        assert_eq!(
            s,
            "CREATE VECTOR INDEX `v` ON `b`.`s`.`c`(`emb` VECTOR) INCLUDE (`cat`) WITH {\"defer_build\": true, \"dimension\": 3, \"similarity\": \"COSINE\"};"
        );
        let (s, _) = deferred("create index i if not exists on `b`(`cat`, `emb` VECTOR)").unwrap();
        assert_eq!(
            s,
            "create index i if not exists on `b`(`cat`, `emb` VECTOR) WITH {\"defer_build\": true};"
        );
        let (s, _) = deferred("CREATE INDEX i ON k(`emb` VECTOR) WITH { }").unwrap();
        assert!(s.ends_with("WITH {\"defer_build\": true};"), "{s}");
        // Already deferred, not a vector index, not an index.
        assert!(
            deferred("CREATE INDEX i ON k(`emb` VECTOR) WITH {\"defer_build\": true}").is_none()
        );
        assert!(deferred("CREATE INDEX i ON k(`cat`)").is_none());
        assert!(deferred("SELECT 1").is_none());
    }

    fn fts_body_sample() -> Value {
        json!({ "indexDefs": { "indexDefs": {
            "b.inv.vs": { "name": "b.inv.vs", "uuid": "u1", "sourceName": "b", "sourceUUID": "x", "type": "fulltext-index",
                "planParams": { "indexPartitions": 1 },
                "params": { "doc_config": { "mode": "scope.collection.type_field" }, "mapping": {
                    "default_mapping": { "enabled": false },
                    "types": {
                        "inv.docs": { "enabled": true, "dynamic": false, "properties": {
                            "emb": { "enabled": true, "fields": [{ "name": "emb", "type": "vector", "dims": 3, "similarity": "dot_product", "vector_index_optimized_for": "recall", "index": true }] },
                            "title": { "fields": [{ "name": "title", "type": "text" }] } } },
                        "inv.other": { "enabled": true, "properties": {
                            "e": { "fields": [{ "type": "vector_base64", "dims": 2, "similarity": "l2_norm" }] } } }
                    } } } },
            "text": { "sourceName": "b", "params": { "mapping": { "types": { "inv.docs": { "properties": {
                "title": { "fields": [{ "type": "text" }] } } } } } } },
            "elsewhere": { "sourceName": "z", "params": {} },
        } } })
    }

    #[test]
    fn search_vector_indexes_join_their_collections() {
        let all = fts_indexes(&fts_body_sample(), "b");
        assert_eq!(all.len(), 2);
        let (s, c, i) = &all[0];
        assert_eq!((s.as_str(), c.as_str()), ("inv", "docs"));
        assert_eq!(i.name, "vs");
        assert_eq!(i.kind.as_deref(), Some(SEARCH));
        assert_eq!(i.columns, ["emb"]);
        let v = i.vector.as_ref().unwrap();
        assert_eq!(v.metric.as_deref(), Some(metric::IP));
        assert_eq!(
            v.params.get("optimized_for").map(String::as_str),
            Some("recall")
        );
        assert_eq!(i.options.get(DIMENSION).map(String::as_str), Some("3"));
        assert_eq!(
            i.options.get(ALSO_MAPS).map(String::as_str),
            Some("inv.other")
        );
        // The definition: this collection's mapping, no server ids.
        let d: Value = serde_json::from_str(&i.options["definition"]).unwrap();
        assert!(
            d.get("uuid").is_none() && d.get("sourceUUID").is_none() && d.get("name").is_none()
        );
        assert_eq!(
            d["params"]["mapping"]["types"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["inv.docs"]
        );
        assert_eq!(all[1].1, "other");
        assert_eq!(
            all[1].2.vector.as_ref().unwrap().metric.as_deref(),
            Some(metric::L2)
        );
        // Describe types the field.
        let mut t = TableSchema {
            name: "docs".into(),
            indexes: vec![i.clone()],
            ..Default::default()
        };
        describe(&mut t);
        assert_eq!(t.columns[0].data_type, "vector(3)");
        // Results mark the array field, not the base64 one.
        let f = fts_result_fields(&fts_body_sample(), "b");
        assert_eq!(
            f.get("emb"),
            Some(&VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert!(!f.contains_key("e"));
    }

    #[test]
    fn search_vector_indexes_are_scripted_for_their_collection() {
        let (_, _, mut i) = fts_indexes(&fts_body_sample(), "b").remove(0);
        // Cloned to another collection, with another dimension and metric.
        i.options.insert(DIMENSION.into(), "4".into());
        i.vector.as_mut().unwrap().metric = Some(metric::L2.into());
        let s = fts_ddl("`b`.`inv`.`copy`", &i, false).unwrap();
        let (comment, stmt) = s.split_once('\n').unwrap();
        assert!(
            comment.starts_with("-- ") && comment.contains("inv.other"),
            "{comment}"
        );
        let Some(Ok(SearchCommand::Create {
            name,
            keyspace,
            definition,
            if_not_exists,
        })) = search_command(stmt)
        else {
            panic!("{stmt}");
        };
        assert_eq!((name.as_str(), if_not_exists), ("vs", false));
        assert_eq!(keyspace, ["b", "inv", "copy"]);
        let t = &definition["params"]["mapping"]["types"];
        assert_eq!(
            t.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["inv.copy"]
        );
        let f = &t["inv.copy"]["properties"]["emb"]["fields"][0];
        assert_eq!(
            (f["dims"].as_u64(), f["similarity"].as_str()),
            (Some(4), Some("l2_norm"))
        );
        assert_eq!(f["vector_index_optimized_for"], "recall");
        assert!(t["inv.copy"]["properties"]["title"].is_object());
        // The same index reads back the same: compare finds nothing.
        let again = fts_ddl("`b`.`inv`.`copy`", &i, false).unwrap();
        assert_eq!(s, again);
        // A renamed field: its mapping moves.
        let mut r = i.clone();
        r.columns = vec!["vec".into()];
        let s = fts_ddl("`b`.`inv`.`docs`", &r, true).unwrap();
        let stmt = s.lines().last().unwrap();
        assert!(
            stmt.starts_with("CREATE SEARCH INDEX `vs` IF NOT EXISTS ON"),
            "{stmt}"
        );
        let Some(Ok(SearchCommand::Create { definition, .. })) = search_command(stmt) else {
            panic!()
        };
        let props = &definition["params"]["mapping"]["types"]["inv.docs"]["properties"];
        assert!(props.get("emb").is_none() && props["vec"]["fields"][0]["type"] == "vector");
        // From the designer: a mapping of just the field.
        let d = IndexDef {
            name: "n".into(),
            columns: vec!["meta.e".into()],
            kind: Some(SEARCH.into()),
            vector: Some(VectorIndex {
                metric: Some("cosine".into()),
                ..Default::default()
            }),
            options: [(DIMENSION.to_string(), "8".to_string())].into(),
            ..Default::default()
        };
        let s = fts_ddl("`b`.`s`.`c`", &d, false).unwrap();
        let Some(Ok(SearchCommand::Create { definition, .. })) = search_command(&s) else {
            panic!("{s}")
        };
        let mut fields = Vec::new();
        fts_fields(
            &definition["params"]["mapping"]["types"]["s.c"]["properties"],
            "",
            &mut fields,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(
            (
                fields[0].path.as_str(),
                fields[0].dims,
                fields[0].similarity.as_deref()
            ),
            ("meta.e", Some(8), Some("cosine"))
        );
        // No dimension, a metric the service lacks: refused.
        let mut nodims = d.clone();
        nodims.options.clear();
        assert!(fts_ddl("`b`.`s`.`c`", &nodims, false).is_err());
        let mut jac = d.clone();
        jac.vector.as_mut().unwrap().metric = Some("jaccard".into());
        assert!(fts_ddl("`b`.`s`.`c`", &jac, false).is_err());
        assert_eq!(
            fts_drop("`b`.`s`.`c`", "n"),
            "DROP SEARCH INDEX `n` ON `b`.`s`.`c`;"
        );
    }

    #[test]
    fn search_statements_parse() {
        assert_eq!(
            search_command("drop search index if exists `a``b` on b.s.c;")
                .unwrap()
                .unwrap(),
            SearchCommand::Drop {
                name: "a`b".into(),
                if_exists: true,
                keyspace: vec!["b".into(), "s".into(), "c".into()],
            }
        );
        assert!(search_command("CREATE SEARCH INDEX x ON b.s.c")
            .unwrap()
            .is_err());
        assert!(search_command("CREATE SEARCH INDEX x ON b.s.c WITH [1]")
            .unwrap()
            .is_err());
        assert!(search_command("CREATE INDEX x ON b(c)").is_none());
        assert!(search_command("SELECT 'CREATE SEARCH INDEX'").is_none());
        assert_eq!(keyspace_parts("`b.x`.`s`.c"), ["b.x", "s", "c"]);
        assert_eq!(
            bucket_scope(&["b".into(), "s".into(), "c".into()], None).unwrap(),
            ("b".to_string(), "s".to_string())
        );
        assert_eq!(
            bucket_scope(&["c".into()], Some("app")).unwrap(),
            ("app".to_string(), "_default".to_string())
        );
        let body = fts_body(&json!({ "params": {} }), "vs", "b");
        assert_eq!(
            (
                body["name"].as_str(),
                body["sourceName"].as_str(),
                body["type"].as_str()
            ),
            (Some("vs"), Some("b"), Some("fulltext-index"))
        );
    }
}
