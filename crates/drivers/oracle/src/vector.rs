//! Oracle's `VECTOR(dims|*, FLOAT32|FLOAT64|INT8|BINARY|*, DENSE|SPARSE)`
//! (Oracle Database 23ai and later) and its vector indexes.
//!
//! - **Cells:** a dense vector is its JSON-array text (`[1,2.5,-3]`),
//!   numbers written the shortest way. A BINARY vector is one 0 or 1 per
//!   dimension (`[1,1,1,1,1,1,1,1,0,0,0,0,0,0,0,0]`), the form every
//!   engine's bit vectors share, unpacked from Oracle's own text (its
//!   bytes, `[255,0]` for those 16 dimensions, the first dimension the
//!   first byte's high bit); loads and edits pack it back. A SPARSE vector
//!   keeps Oracle's text `[dims,[indices],[values]]`, indices from 0.
//! - **Types:** the dictionary's `VECTOR_INFO` (`VECTOR(3,FLOAT32,DENSE)`)
//!   gives the declared type; a result column's metadata gives the same
//!   from the client.
//! - **Literals:** the text in a string, which the server converts to the
//!   column's type (a `TO_VECTOR` would make every one FLOAT32 and refuse
//!   BINARY and SPARSE); past 4000 bytes, as a `TO_CLOB(…) || …` chain.
//! - **Indexes:** `CREATE VECTOR INDEX … ORGANIZATION INMEMORY NEIGHBOR
//!   GRAPH` (HNSW) or `NEIGHBOR PARTITIONS` (IVF), `DISTANCE …`, `WITH
//!   TARGET ACCURACY n`, `PARAMETERS (TYPE HNSW, NEIGHBORS m,
//!   EFCONSTRUCTION ef | TYPE IVF, NEIGHBOR PARTITIONS n)`, `INCLUDE (…)`.

use dbine_driver::vector::{self, metric, param, VectorElement, VectorIndex, VectorMeta};
use std::collections::BTreeMap;

/// `IndexDef::kind` of a vector index.
pub const KIND: &str = "VECTOR";

/// The tables (and their indexes) Oracle makes for each vector index in
/// the index owner's schema: `VECTOR$<index>$<objn>_<objn>_<n>$HNSW_…`
/// (checkpoints, rowid map, journal) and `…$IVF_FLAT_CENTROIDS` /
/// `…_CENTROID_PARTITIONS`, the index name cut short when it's long. The
/// dictionary doesn't flag them (`SECONDARY`, `GENERATED`, `NESTED` are
/// all "no"), so every listing (tables, explorer, statistics, health,
/// search) leaves out the names matching this `REGEXP_LIKE` pattern,
/// written in each query: they're part of the index, recreated with it,
/// and their object numbers never match between two databases. The tests
/// check that every listing has it.
#[cfg(test)]
pub(crate) const INTERNAL_TABLES: &str = "'^VECTOR\\$.+\\$[0-9]+_[0-9]+_[0-9]+\\$'";

/// Whether `ALL_TAB_COLS` / `ALL_TAB_COLUMNS` have `VECTOR_INFO` (each
/// vector column's declared type); asked once per connection, kept in
/// `cache` (0 not asked, 1 yes, 2 no).
pub(crate) fn info_column(c: &oracledb::Connection, cache: &std::sync::atomic::AtomicU8) -> bool {
    use std::sync::atomic::Ordering;
    match cache.load(Ordering::Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    let has = c.version().is_ok_and(|v| v.0 >= 23)
        && c.query_row(
            "SELECT COUNT(*) FROM all_tab_columns
              WHERE owner = 'SYS' AND table_name = 'ALL_TAB_COLS' AND column_name = 'VECTOR_INFO'",
            &[],
        )
        .and_then(|r| r.get::<i64>(0))
        .is_ok_and(|n| n > 0);
    cache.store(if has { 1 } else { 2 }, Ordering::Relaxed);
    has
}

/// The select-list item with a column's `VECTOR_INFO` (`c` its alias), or
/// a NULL where the dictionary doesn't have it.
pub(crate) fn info_expr(has: bool) -> &'static str {
    if has {
        "c.vector_info"
    } else {
        "CAST(NULL AS VARCHAR2(1))"
    }
}

/// The declared type of a column whose `DATA_TYPE` is `VECTOR`, from its
/// `VECTOR_INFO`; a bare `VECTOR` without it.
pub(crate) fn declared(info: Option<&str>) -> String {
    info.and_then(Shape::parse)
        .map_or_else(|| "VECTOR".to_string(), |s| s.ddl())
}

/// The engine's name for a vector index's target accuracy, in
/// `VectorIndex::params`.
pub const TARGET_ACCURACY: &str = "target_accuracy";

/// Longest string literal SQL takes (bytes).
const MAX_LITERAL: usize = 4000;

/// A declared vector type, as the dictionary or the client says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shape {
    /// `None`: `*` (any).
    pub dims: Option<u32>,
    /// `FLOAT32`, `FLOAT64`, `INT8`, `BINARY`; `None`: `*`.
    pub format: Option<String>,
    pub sparse: bool,
}

impl Shape {
    /// `VECTOR(3,FLOAT32,DENSE)` (`ALL_TAB_COLS.VECTOR_INFO`), `VECTOR(3,
    /// float32)` / `VECTOR(*,float32)` (the client), `VECTOR(3)`, `VECTOR`.
    pub(crate) fn parse(t: &str) -> Option<Shape> {
        let t = t.trim();
        if !t.get(..6)?.eq_ignore_ascii_case("VECTOR") {
            return None;
        }
        let inner = t[6..].trim();
        if inner.is_empty() {
            return Some(Shape {
                dims: None,
                format: None,
                sparse: false,
            });
        }
        let inner = inner.strip_prefix('(')?.strip_suffix(')')?;
        let mut parts = inner.split(',').map(|p| p.trim().to_ascii_uppercase());
        let dims = match parts.next()?.as_str() {
            "*" | "" => None,
            n => Some(n.parse().ok()?),
        };
        let format = match parts.next().as_deref() {
            None | Some("*") => None,
            Some(f @ ("FLOAT32" | "FLOAT64" | "INT8" | "BINARY")) => Some(f.to_string()),
            Some(_) => return None,
        };
        let sparse = match parts.next().as_deref() {
            None | Some("DENSE") => false,
            Some("SPARSE") => true,
            Some(_) => return None,
        };
        parts.next().is_none().then_some(Shape {
            dims,
            format,
            sparse,
        })
    }

    /// As DDL declares it: `VECTOR(3, FLOAT32)`, `VECTOR(*, *)`,
    /// `VECTOR(5, FLOAT32, SPARSE)`.
    pub(crate) fn ddl(&self) -> String {
        let dims = self.dims.map_or("*".to_string(), |n| n.to_string());
        let format = self.format.as_deref().unwrap_or("*");
        if self.sparse {
            format!("VECTOR({dims}, {format}, SPARSE)")
        } else {
            format!("VECTOR({dims}, {format})")
        }
    }

    pub(crate) fn element(&self) -> VectorElement {
        match self.format.as_deref() {
            Some("FLOAT64") => VectorElement::F64,
            Some("INT8") => VectorElement::I8,
            Some("BINARY") => VectorElement::Bit,
            // FLOAT32, and `*` (stored as given, usually float32).
            _ => VectorElement::F32,
        }
    }

    pub(crate) fn meta(&self) -> VectorMeta {
        VectorMeta {
            dims: self.dims,
            element: self.element(),
            sparse: self.sparse,
        }
    }
}

/// A result column's shape from the client's metadata
/// (`Metadata::data_type`: `VECTOR(3,float32)`, `VECTOR(*,float32)`, `VECTOR`).
pub(crate) fn column_shape(data_type: &str, sparse: bool) -> Shape {
    let mut s = Shape::parse(data_type).unwrap_or(Shape {
        dims: None,
        format: None,
        sparse: false,
    });
    s.sparse = sparse;
    s
}

/// A vector value as its cell / transfer text.
pub(crate) fn text(v: &oracledb::Vector) -> String {
    match v {
        oracledb::Vector::Dense(d) => dense(d),
        oracledb::Vector::Sparse(s) => format!(
            "[{},{},{}]",
            s.num_dimensions(),
            vector::text(s.indices().iter().map(|&i| i as f64)),
            dense(s.values())
        ),
    }
}

fn dense(d: &oracledb::VectorData) -> String {
    use oracledb::VectorData as D;
    match d {
        D::Float32(x) => vector::text(x.iter().copied()),
        D::Float64(x) => vector::text(x.iter().copied()),
        D::Int8(x) => vector::text(x.iter().copied()),
        // One 0 / 1 per dimension, the first one the first byte's high bit.
        D::Binary(x) => vector::text(vector::from_bits(x, None)),
    }
}

/// The values of a vector that may be a BINARY one: a JSON array (or a
/// bit string, `0101…`, as pgvector's `bit` writes it) of 0s and 1s, in
/// whole bytes.
fn bits(t: &str) -> Option<Vec<f64>> {
    let t = t.trim();
    let v = if !t.is_empty() && t.bytes().all(|b| b == b'0' || b == b'1') {
        t.bytes().map(|b| f64::from(b - b'0')).collect()
    } else {
        vector::parse(t)?
    };
    (!v.is_empty() && v.len() % 8 == 0 && v.iter().all(|&b| b == 0.0 || b == 1.0)).then_some(v)
}

/// A vector's text for a `VECTOR(dims, BINARY)` column: Oracle's packed
/// bytes from one 0 / 1 per dimension. Anything else (bytes already, a
/// count that doesn't fit) goes as it is, for the server to take or
/// refuse.
pub(crate) fn binary_text(t: &str, dims: Option<u32>) -> Option<String> {
    let v = bits(t)?;
    match dims {
        Some(d) if v.len() != d as usize => None,
        _ => Some(vector::text(vector::to_bits(&v))),
    }
}

/// The dimensions of a declared `VECTOR(n, BINARY)` (`Some(None)`: `*`),
/// or `None` when the column isn't a BINARY vector.
pub(crate) fn binary_dims(declared: &str) -> Option<Option<u32>> {
    let s = Shape::parse(declared)?;
    (s.format.as_deref() == Some("BINARY") && !s.sparse).then_some(s.dims)
}

/// A vector edit's value as a literal: its text in a string the server
/// converts to the column's type, chained through `TO_CLOB` past the
/// 4000-byte limit of a literal. `None` when `v` isn't a vector.
pub(crate) fn literal(v: &serde_json::Value) -> Option<String> {
    vector::value_text(v).map(|t| text_literal(&t))
}

/// A vector edit's literal for `column` of `table` (`owner`: its schema,
/// `None` the current one) when the value may be a BINARY vector's, one
/// 0 / 1 per dimension: only the column's declared type tells whether
/// Oracle wants those as packed bytes (BINARY) or as numbers (any other
/// format), and the edit scripts don't get it, so the statement asks the
/// dictionary. `None` for any other value (see [`literal`]).
pub(crate) fn column_literal(
    owner: Option<&str>,
    table: &str,
    column: &str,
    v: &serde_json::Value,
) -> Option<String> {
    let plain = vector::value_text(v)?;
    let packed = binary_text(&plain, None)?;
    let plain = text_literal(&plain);
    let packed = if plain.starts_with("TO_CLOB(") {
        format!("TO_CLOB({})", text_literal(&packed))
    } else {
        text_literal(&packed)
    };
    let s = |x: &str| format!("'{}'", x.replace('\'', "''"));
    let owner = owner
        .filter(|o| !o.is_empty())
        .map_or_else(|| "SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')".to_string(), s);
    Some(format!(
        "(SELECT CASE WHEN MAX(vector_info) LIKE '%BINARY%' THEN {packed} ELSE {plain} END \
         FROM all_tab_cols WHERE owner = {owner} AND table_name = {} AND column_name = {})",
        s(table),
        s(column)
    ))
}

/// A result's or a describe's column as the vector type it declares: its
/// type name (`VECTOR(16, BINARY)`, `VECTOR(5, FLOAT32, SPARSE)`), else
/// what its vector meta says. `None` when it isn't a vector column.
pub(crate) fn shape_of(c: &dbine_driver::ResultColumn) -> Option<Shape> {
    if let Some(s) = Shape::parse(&c.type_name) {
        return Some(s);
    }
    let m = c.vector.as_ref()?;
    let format = match m.element {
        VectorElement::F32 => Some("FLOAT32"),
        VectorElement::F64 => Some("FLOAT64"),
        VectorElement::I8 => Some("INT8"),
        VectorElement::Bit if !m.sparse => Some("BINARY"),
        _ => None,
    };
    Some(Shape {
        dims: m.dims,
        format: format.map(str::to_string),
        sparse: m.sparse,
    })
}

/// A vector edit's literal for a column of the vector type `shape`, from
/// its numbers or its text: a BINARY column's 0 / 1 per dimension packed
/// into Oracle's bytes, a SPARSE column's dense or pgvector-style value
/// (`{1:1.5,3:3}/5`) in Oracle's sparse text and a dense column's sparse
/// one spread out (Oracle converts neither: ORA-51833), any length. A
/// vector that needs no change goes in its canonical text. `None` for a
/// value that isn't a vector's (a number, a boolean, text that doesn't
/// read as a vector), which the caller writes as a plain literal: the
/// text of a grid, a file or another database never reaches the
/// statement as it came.
pub(crate) fn typed_literal(shape: &Shape, v: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    let text = match v {
        Value::Null => return Some("NULL".into()),
        Value::String(s) => s.trim().to_string(),
        // Numbers in the canonical text; a nested array (a sparse vector's
        // parts) as its JSON.
        Value::Array(_) => vector::value_text(v).unwrap_or_else(|| v.to_string()),
        _ => return None,
    };
    let text = if shape.sparse {
        sparse_text(&text, shape.dims)
    } else if shape.format.as_deref() == Some("BINARY") {
        binary_text(&text, shape.dims)
    } else {
        dense_text(&text)
    }
    .or_else(|| canonical(&text))?;
    Some(text_literal(&text))
}

/// A vector's text as it is, in the canonical form: a dense one's numbers
/// written the shortest way, a sparse one in Oracle's text. `None` when
/// the text isn't a vector's.
fn canonical(t: &str) -> Option<String> {
    if let Some(dense) = vector::parse(t) {
        return Some(vector::text(dense));
    }
    let (n, idx, val) = sparse_parts(t)?;
    Some(format!(
        "[{n},{},{}]",
        vector::text(idx.iter().map(|&i| f64::from(i))),
        vector::text(val)
    ))
}

/// A sparse vector's parts: dimensions, indices from 0 and values, from
/// Oracle's text (`[5,[0,2],[1.5,3]]`) or pgvector's (`{1:1.5,3:3}/5`,
/// indices from 1).
fn sparse_parts(t: &str) -> Option<(u32, Vec<u32>, Vec<f64>)> {
    let t = t.trim();
    if let Some(rest) = t.strip_prefix('{') {
        let (pairs, dims) = rest.split_once("}/")?;
        let dims: u32 = dims.trim().parse().ok()?;
        let (mut idx, mut val) = (Vec::new(), Vec::new());
        for p in pairs.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (i, x) = p.split_once(':')?;
            let i: u32 = i.trim().parse().ok()?;
            idx.push(i.checked_sub(1)?);
            val.push(x.trim().parse::<f64>().ok().filter(|x| x.is_finite())?);
        }
        return Some((dims, idx, val));
    }
    let v: serde_json::Value = serde_json::from_str(t).ok()?;
    let [dims, idx, val] = v.as_array()?.as_slice() else {
        return None;
    };
    let dims = u32::try_from(dims.as_u64()?).ok()?;
    let idx: Option<Vec<u32>> = idx
        .as_array()?
        .iter()
        .map(|i| i.as_u64().and_then(|i| u32::try_from(i).ok()))
        .collect();
    let val = vector::parse_value(val)?;
    let idx = idx?;
    (idx.len() == val.len()).then_some((dims, idx, val))
}

/// A dense vector (or pgvector's sparse text) as Oracle's sparse text,
/// `[dims,[indices],[values]]` with the non-zero values; `None` when it's
/// already one, or isn't a vector of `dims` dimensions.
pub(crate) fn sparse_text(t: &str, dims: Option<u32>) -> Option<String> {
    let (n, idx, val) = match vector::parse(t) {
        Some(dense) => {
            let n = u32::try_from(dense.len()).ok()?;
            let (idx, val): (Vec<u32>, Vec<f64>) = dense
                .iter()
                .enumerate()
                .filter(|(_, x)| **x != 0.0)
                .map(|(i, x)| (i as u32, *x))
                .unzip();
            (n, idx, val)
        }
        None if t.trim_start().starts_with('{') => sparse_parts(t)?,
        None => return None,
    };
    if dims.is_some_and(|d| d != n) {
        return None;
    }
    Some(format!(
        "[{n},{},{}]",
        vector::text(idx.iter().map(|&i| f64::from(i))),
        vector::text(val)
    ))
}

/// A sparse vector's text (Oracle's or pgvector's) spread out as a dense
/// one; `None` for anything else.
pub(crate) fn dense_text(t: &str) -> Option<String> {
    if vector::parse(t).is_some() {
        return None;
    }
    let (n, idx, val) = sparse_parts(t)?;
    let mut dense = vec![0.0; n as usize];
    for (i, x) in idx.into_iter().zip(val) {
        *dense.get_mut(i as usize)? = x;
    }
    Some(vector::text(dense))
}

/// Any text as a string literal: `'text'`, or past the 4000 bytes a
/// literal takes, `TO_CLOB('…') || '…'` in chunks. Every chunk has its
/// quotes doubled, fits the limit with them, and ends on a whole
/// character.
pub(crate) fn text_literal(t: &str) -> String {
    let quoted = |c: &str| format!("'{}'", c.replace('\'', "''"));
    // Bytes in the literal: a quote is written twice.
    let size = |c: char| if c == '\'' { 2 } else { c.len_utf8() };
    if t.chars().map(size).sum::<usize>() <= MAX_LITERAL {
        return quoted(t);
    }
    let mut chunks = Vec::new();
    let (mut start, mut len) = (0, 0);
    for (i, c) in t.char_indices() {
        if len + size(c) > MAX_LITERAL {
            chunks.push(&t[start..i]);
            (start, len) = (i, 0);
        }
        len += size(c);
    }
    chunks.push(&t[start..]);
    let mut s = format!("TO_CLOB({})", quoted(chunks[0]));
    for c in &chunks[1..] {
        s.push_str(&format!("\n  || {}", quoted(c)));
    }
    s
}

/// Text for a `--` comment line: line breaks can't end it early.
pub(crate) fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}') {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// What a script says before a vector index without a distance: Oracle
/// makes it with its defaults (COSINE, its own accuracy and parameters),
/// which may not be the original's when the settings couldn't be read. On
/// a BINARY column (`binary`) the script says HAMMING instead: Oracle
/// refuses COSINE there (ORA-51910).
pub(crate) fn unknown_settings_note(index: &str, binary: bool) -> String {
    let made = if binary {
        "sobre un vector BINARY Oracle solo admite HAMMING y JACCARD, así que se crea con HAMMING y los valores por defecto de Oracle"
    } else {
        "Oracle lo crea con COSINE y sus valores por defecto"
    };
    format!(
        "-- El índice vectorial {} no indica la distancia: {made}. \
         Si viene de otra base, DBine no pudo leer su configuración: hace falta ser el dueño del índice o tener el rol SELECT_CATALOG_ROLE.",
        comment_text(index)
    )
}

/// A statement with a vector type or index that a server before 23ai
/// refused (`server`: its major release; 0 unknown, no hint): the error
/// says the release that has them, since the designer offers them on every
/// server.
pub(crate) fn with_release_hint(
    e: dbine_driver::Error,
    server: u32,
    statement: &str,
) -> dbine_driver::Error {
    match e {
        dbine_driver::Error::Statement(mut se)
            if (1..23).contains(&server) && mentions_vectors(statement) =>
        {
            se.message.push_str(&format!(
                " — Oracle {server} no tiene el tipo VECTOR ni índices vectoriales: llegaron en Oracle Database 23ai."
            ));
            dbine_driver::Error::Statement(se)
        }
        other => other,
    }
}

/// Whether a statement declares a vector column or makes a vector index.
fn mentions_vectors(statement: &str) -> bool {
    let up = statement.to_ascii_uppercase();
    up.contains("VECTOR(") || up.contains("VECTOR (") || up.contains("VECTOR INDEX")
}

/// Oracle's distance names and the standard ones.
pub(crate) fn metric_of(native: &str) -> String {
    match native.trim().to_ascii_uppercase().as_str() {
        "COSINE" => metric::COSINE.into(),
        "EUCLIDEAN" | "L2_DISTANCE" => metric::L2.into(),
        "DOT" | "INNER_PRODUCT" => metric::IP.into(),
        "MANHATTAN" | "L1_DISTANCE" => metric::L1.into(),
        "HAMMING" => metric::HAMMING.into(),
        "JACCARD" => metric::JACCARD.into(),
        other => other.to_ascii_lowercase(),
    }
}

/// The `DISTANCE …` of a standard (or Oracle) distance name.
pub(crate) fn native_metric(m: &str) -> Option<&'static str> {
    Some(match m.trim().to_ascii_lowercase().as_str() {
        metric::COSINE => "COSINE",
        metric::L2 | "euclidean" => "EUCLIDEAN",
        "euclidean_squared" | "l2_squared" => "EUCLIDEAN_SQUARED",
        metric::IP | "dot" => "DOT",
        metric::L1 | "manhattan" => "MANHATTAN",
        metric::HAMMING => "HAMMING",
        metric::JACCARD => "JACCARD",
        _ => return None,
    })
}

/// The algorithm of `ALL_INDEXES.INDEX_SUBTYPE` / `V$VECTOR_INDEX`'s
/// organization.
pub(crate) fn algorithm_of(subtype: &str) -> Option<&'static str> {
    let s = subtype.to_ascii_uppercase();
    if s.contains("HNSW") || s.contains("NEIGHBOR GRAPH") || s.contains("NEIGHBOR_GRAPH") {
        Some("hnsw")
    } else if s.contains("IVF") || s.contains("PARTITIONS") {
        Some("ivf")
    } else {
        None
    }
}

/// What the dictionary says about one vector index (`KU$_VECTOR_INDEX_VIEW`
/// or `VECSYS.VECTOR$INDEX`'s parameters, or `V$VECTOR_INDEX`'s distance
/// and accuracy).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Details {
    pub distance: Option<String>,
    pub accuracy: Option<i64>,
    pub neighbors: Option<i64>,
    pub ef_construction: Option<i64>,
    /// IVF: the partitions asked for (`target_centroids`).
    pub partitions: Option<i64>,
}

impl Details {
    /// From `KU$_VECTOR_INDEX_VIEW`'s `type, distance, accuracy,
    /// num_neighbors, efconstruction, target_centroids`, as text.
    pub(crate) fn from_metadata(r: &[Option<String>]) -> Details {
        let n = |i: usize| {
            r.get(i)
                .cloned()
                .flatten()
                .and_then(|v| v.trim().parse::<i64>().ok())
        };
        Details {
            distance: r
                .get(1)
                .cloned()
                .flatten()
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty()),
            accuracy: n(2),
            neighbors: n(3),
            ef_construction: n(4),
            partitions: n(5),
        }
    }

    /// From `VECSYS.VECTOR$INDEX.IDX_PARAMS` (JSON).
    pub(crate) fn from_params(json: &str) -> Details {
        let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
        let n = |k: &str| v.get(k).and_then(serde_json::Value::as_i64);
        Details {
            distance: v
                .get("distance")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            accuracy: n("accuracy"),
            neighbors: n("num_neighbors"),
            ef_construction: n("efConstruction"),
            partitions: n("target_centroids"),
        }
    }
}

/// The index's [`VectorIndex`]: standard metric and parameter names.
pub(crate) fn index(algorithm: Option<&str>, d: &Details) -> VectorIndex {
    let mut params = BTreeMap::new();
    if let Some(a) = d.accuracy {
        params.insert(TARGET_ACCURACY.to_string(), a.to_string());
    }
    match algorithm {
        Some("hnsw") => {
            if let Some(m) = d.neighbors {
                params.insert(param::M.to_string(), m.to_string());
            }
            if let Some(ef) = d.ef_construction {
                params.insert(param::EF_CONSTRUCTION.to_string(), ef.to_string());
            }
        }
        Some("ivf") => {
            if let Some(p) = d.partitions {
                params.insert(param::LISTS.to_string(), p.to_string());
            }
        }
        _ => {}
    }
    VectorIndex {
        algorithm: algorithm.map(str::to_string),
        metric: d.distance.as_deref().map(metric_of),
        params,
    }
}

/// `CREATE VECTOR INDEX`. `cols`: the vector column, then the INCLUDE
/// ones, quoted. `Err` with why it can't be written.
pub(crate) fn index_sql(
    table: &str,
    name: &str,
    cols: &[String],
    v: &VectorIndex,
) -> Result<String, String> {
    let (first, include) = cols
        .split_first()
        .ok_or_else(|| "el índice vectorial no tiene columna".to_string())?;
    let algorithm = v
        .algorithm
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "hnsw".into());
    let organization = match algorithm.as_str() {
        "hnsw" => "INMEMORY NEIGHBOR GRAPH",
        "ivf" | "ivfflat" | "ivf_flat" => "NEIGHBOR PARTITIONS",
        other => {
            return Err(format!(
                "Oracle no tiene el algoritmo «{other}» (HNSW o IVF)"
            ))
        }
    };
    let mut s = format!("CREATE VECTOR INDEX {name} ON {table} ({first})");
    if !include.is_empty() {
        s.push_str(&format!(" INCLUDE ({})", include.join(", ")));
    }
    s.push_str(&format!(" ORGANIZATION {organization}"));
    if let Some(m) = v.metric.as_deref() {
        let d = native_metric(m).ok_or_else(|| format!("Oracle no tiene la distancia «{m}»"))?;
        s.push_str(&format!(" DISTANCE {d}"));
    }
    let p = |k: &str| {
        v.params
            .get(k)
            .map(|x| x.trim())
            .filter(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
    };
    if let Some(a) = p(TARGET_ACCURACY) {
        s.push_str(&format!(" WITH TARGET ACCURACY {a}"));
    }
    let mut params = Vec::new();
    if organization.starts_with("INMEMORY") {
        if let Some(m) = p(param::M) {
            params.push(format!("NEIGHBORS {m}"));
        }
        if let Some(ef) = p(param::EF_CONSTRUCTION) {
            params.push(format!("EFCONSTRUCTION {ef}"));
        }
        if !params.is_empty() {
            params.insert(0, "TYPE HNSW".into());
        }
    } else if let Some(n) = p(param::LISTS) {
        params.push("TYPE IVF".into());
        params.push(format!("NEIGHBOR PARTITIONS {n}"));
    }
    if !params.is_empty() {
        s.push_str(&format!(" PARAMETERS ({})", params.join(", ")));
    }
    Ok(s)
}

/// What the designer offers: HNSW (in memory) and IVF, every distance,
/// HNSW's neighbors and efConstruction, IVF's partitions, the target
/// accuracy, and INCLUDE columns.
pub(crate) fn spec() -> dbine_driver::VectorIndexSpec {
    use dbine_driver::VectorParamSpec as P;
    dbine_driver::VectorIndexSpec {
        algorithms: vec!["hnsw", "ivf"],
        metrics: vec![
            metric::COSINE,
            metric::L2,
            metric::IP,
            metric::L1,
            metric::HAMMING,
            metric::JACCARD,
        ],
        params: vec![
            P {
                key: param::M,
                algorithms: vec!["hnsw"],
                default: "32",
            },
            P {
                key: param::EF_CONSTRUCTION,
                algorithms: vec!["hnsw"],
                default: "300",
            },
            P {
                key: param::LISTS,
                algorithms: vec!["ivf"],
                default: "",
            },
            P {
                key: TARGET_ACCURACY,
                algorithms: vec![],
                default: "90",
            },
        ],
        filter_columns: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn typed_literals_follow_the_columns_format() {
        use dbine_driver::ResultColumn;
        let shape = |t: &str| Shape::parse(t).unwrap();
        // The type name says it, else the vector meta.
        assert_eq!(
            shape_of(&ResultColumn::new("B", "VECTOR(16, BINARY)")),
            Some(shape("VECTOR(16, BINARY)"))
        );
        assert_eq!(
            shape_of(
                &ResultColumn::new("B", "")
                    .with_vector(VectorMeta::dense(Some(16), VectorElement::Bit))
            ),
            Some(shape("VECTOR(16, BINARY)"))
        );
        assert_eq!(
            shape_of(
                &ResultColumn::new("S", "")
                    .with_vector(VectorMeta::sparse(Some(5), VectorElement::F32))
            ),
            Some(shape("VECTOR(5, FLOAT32, SPARSE)"))
        );
        assert_eq!(shape_of(&ResultColumn::new("N", "NUMBER")), None);

        let bin = shape("VECTOR(16, BINARY)");
        let bits = json!([1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(typed_literal(&bin, &bits).as_deref(), Some("'[255,1]'"));
        // As the grid's text too; bytes already packed go as they are.
        assert_eq!(
            typed_literal(&bin, &json!("[1,1,1,1,1,1,1,1,0,0,0,0,0,0,0,1]")).as_deref(),
            Some("'[255,1]'")
        );
        assert_eq!(
            typed_literal(&bin, &json!([255, 1])).as_deref(),
            Some("'[255,1]'")
        );
        assert_eq!(typed_literal(&bin, &Value::Null).as_deref(), Some("NULL"));
        assert_eq!(typed_literal(&bin, &json!(3)), None);

        let sparse = shape("VECTOR(5, FLOAT32, SPARSE)");
        for v in [
            json!([0, 1.5, 0, 0, 3]),
            json!("{2:1.5,5:3}/5"),
            json!("[5,[1,4],[1.5,3]]"),
            json!([5, [1, 4], [1.5, 3]]),
        ] {
            assert_eq!(
                typed_literal(&sparse, &v).as_deref(),
                Some("'[5,[1,4],[1.5,3]]'"),
                "{v}"
            );
        }
        // A dense value of another dimension goes as it is (refused).
        assert_eq!(
            typed_literal(&sparse, &json!([1, 2])).as_deref(),
            Some("'[1,2]'")
        );

        let dense = shape("VECTOR(5, FLOAT32)");
        for v in [json!("[5,[1,4],[1.5,3]]"), json!("{2:1.5,5:3}/5")] {
            assert_eq!(
                typed_literal(&dense, &v).as_deref(),
                Some("'[0,1.5,0,0,3]'"),
                "{v}"
            );
        }
        // Past the literal limit, a CLOB chain.
        let long = json!(vec![0.123456; 1536]);
        let l = typed_literal(&shape("VECTOR(1536, FLOAT64)"), &long).unwrap();
        assert!(l.starts_with("TO_CLOB('[0.123456,"), "{}", &l[..40]);
        // Not a sparse vector: an index past the dimensions, a bad pair.
        assert_eq!(dense_text("[2,[5],[1]]"), None);
        assert_eq!(sparse_text("{0:1}/3", None), None);
        assert_eq!(sparse_text("{1:x}/3", None), None);
        assert_eq!(dense_text("[1,2]"), None);
    }

    #[test]
    fn settings_from_the_data_pump_view() {
        let row = |v: &[&str]| -> Vec<Option<String>> {
            v.iter()
                .map(|s| (!s.is_empty()).then(|| s.to_string()))
                .collect()
        };
        let d = Details::from_metadata(&row(&["HNSW", "DOT", "93", "12", "77", ""]));
        assert_eq!(algorithm_of("HNSW"), Some("hnsw"));
        let v = index(Some("hnsw"), &d);
        assert_eq!(v.metric.as_deref(), Some("ip"));
        assert_eq!(
            (
                v.params.get("m").map(String::as_str),
                v.params.get("ef_construction").map(String::as_str),
                v.params.get(TARGET_ACCURACY).map(String::as_str)
            ),
            (Some("12"), Some("77"), Some("93"))
        );
        let d = Details::from_metadata(&row(&["IVF_FLAT", "HAMMING", "88", "", "", "3"]));
        assert_eq!(algorithm_of("IVF_FLAT"), Some("ivf"));
        let v = index(Some("ivf"), &d);
        assert_eq!(v.metric.as_deref(), Some("hamming"));
        assert_eq!(v.params.get("lists").map(String::as_str), Some("3"));
        assert_eq!(
            index_sql("\"T\"", "\"IX\"", &["\"B\"".into()], &v).unwrap(),
            "CREATE VECTOR INDEX \"IX\" ON \"T\" (\"B\") ORGANIZATION NEIGHBOR PARTITIONS \
             DISTANCE HAMMING WITH TARGET ACCURACY 88 PARAMETERS (TYPE IVF, NEIGHBOR PARTITIONS 3)"
        );
        // Nothing read: no distance, and the note says why.
        assert_eq!(Details::from_metadata(&[]), Details::default());
        let note = unknown_settings_note("IX\nX", false);
        assert!(note.starts_with("-- El índice vectorial IX?X no indica la distancia"));
        assert!(!note.contains('\n'));
    }

    #[test]
    fn shapes_from_the_dictionary_and_the_client() {
        let s = Shape::parse("VECTOR(3,FLOAT32,DENSE)").unwrap();
        assert_eq!(s.ddl(), "VECTOR(3, FLOAT32)");
        assert_eq!(s.meta(), VectorMeta::dense(Some(3), VectorElement::F32));
        let s = Shape::parse("VECTOR(5,FLOAT32,SPARSE)").unwrap();
        assert_eq!(s.ddl(), "VECTOR(5, FLOAT32, SPARSE)");
        assert!(s.meta().sparse);
        assert_eq!(
            Shape::parse("VECTOR(*,*,DENSE)").unwrap().ddl(),
            "VECTOR(*, *)"
        );
        assert_eq!(
            Shape::parse("VECTOR(16,BINARY,DENSE)").unwrap().element(),
            VectorElement::Bit
        );
        assert_eq!(
            Shape::parse("VECTOR(4, int8)").unwrap().ddl(),
            "VECTOR(4, INT8)"
        );
        assert_eq!(
            Shape::parse("VECTOR(2,float64)").unwrap().element(),
            VectorElement::F64
        );
        assert_eq!(Shape::parse("VECTOR").unwrap().ddl(), "VECTOR(*, *)");
        assert_eq!(
            Shape::parse("VECTOR(*,float32)").unwrap().ddl(),
            "VECTOR(*, FLOAT32)"
        );
        assert_eq!(Shape::parse("VARCHAR2(3)"), None);
        assert_eq!(Shape::parse("VECTOR(3,FLOAT16)"), None);
        let c = column_shape("VECTOR(5,float32)", true);
        assert_eq!(c.ddl(), "VECTOR(5, FLOAT32, SPARSE)");
    }

    #[test]
    fn cells_are_short_json_arrays() {
        use oracledb::VectorData as D;
        let v = oracledb::Vector::Dense(D::Float32(vec![1.0, 2.5, 0.1]));
        assert_eq!(text(&v), "[1,2.5,0.1]");
        let v = oracledb::Vector::Dense(D::Float64(vec![1.5, 2.25]));
        assert_eq!(text(&v), "[1.5,2.25]");
        let v = oracledb::Vector::Dense(D::Int8(vec![1, -2, 3]));
        assert_eq!(text(&v), "[1,-2,3]");
        let v = oracledb::Vector::Dense(D::Binary(vec![201, 15]));
        assert_eq!(text(&v), "[1,1,0,0,1,0,0,1,0,0,0,0,1,1,1,1]");
        let s = oracledb::Vector::Sparse(oracledb::SparseVector::new(
            5,
            vec![1, 3],
            D::Float32(vec![0.5, 2.5]),
        ));
        assert_eq!(text(&s), "[5,[1,3],[0.5,2.5]]");
    }

    #[test]
    fn binary_vectors_pack_and_unpack() {
        // Oracle's own example: [201,15] is 1,1,0,0,1,0,0,1, 0,0,0,0,1,1,1,1.
        let bits = "[1,1,0,0,1,0,0,1,0,0,0,0,1,1,1,1]";
        assert_eq!(binary_text(bits, Some(16)).as_deref(), Some("[201,15]"));
        assert_eq!(binary_text(bits, None).as_deref(), Some("[201,15]"));
        assert_eq!(
            binary_text("1100100100001111", Some(16)).as_deref(),
            Some("[201,15]"),
            "pgvector's bit text"
        );
        // Already bytes, or not whole bytes: as they are.
        assert_eq!(binary_text("[201,15]", Some(16)), None);
        assert_eq!(binary_text("[1,0]", Some(16)), None);
        assert_eq!(binary_text("[1,0,1]", None), None);
        assert_eq!(binary_text(bits, Some(24)), None);
        assert_eq!(binary_dims("VECTOR(16, BINARY)"), Some(Some(16)));
        assert_eq!(binary_dims("VECTOR(*,BINARY,DENSE)"), Some(None));
        assert_eq!(binary_dims("VECTOR(16, FLOAT32)"), None);
        assert_eq!(binary_dims("VARCHAR2(10)"), None);
    }

    #[test]
    fn bit_edits_ask_the_dictionary() {
        let bits = json!([1, 1, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 1, 1, 1, 1]);
        assert_eq!(
            column_literal(Some("APP"), "T", "B", &bits).unwrap(),
            "(SELECT CASE WHEN MAX(vector_info) LIKE '%BINARY%' THEN '[201,15]' \
             ELSE '[1,1,0,0,1,0,0,1,0,0,0,0,1,1,1,1]' END FROM all_tab_cols \
             WHERE owner = 'APP' AND table_name = 'T' AND column_name = 'B')"
        );
        assert!(column_literal(None, "T", "B", &bits)
            .unwrap()
            .contains("owner = SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')"));
        // Any other vector is a plain literal.
        assert_eq!(column_literal(None, "T", "V", &json!([0.5, 1.0])), None);
        assert_eq!(column_literal(None, "T", "V", &json!([1, 0, 1])), None);
        assert_eq!(column_literal(None, "T", "V", &json!("x")), None);
        // A long one: both branches are CLOBs.
        let long = json!(vec![1; 4096]);
        let l = column_literal(None, "T", "B", &long).unwrap();
        assert!(l.contains("THEN TO_CLOB('[255,"), "{}", &l[..80]);
        assert!(l.contains("ELSE TO_CLOB('[1,1,"));
    }

    #[test]
    fn older_servers_say_which_release_has_vectors() {
        let e = || {
            dbine_driver::Error::Statement(Box::new(dbine_driver::ScriptError::new(
                "ORA-00902: invalid datatype",
            )))
        };
        let msg = |e: dbine_driver::Error| match e {
            dbine_driver::Error::Statement(se) => se.message,
            other => other.to_string(),
        };
        let ddl = "CREATE TABLE T (V VECTOR(3, FLOAT32))";
        assert!(msg(with_release_hint(e(), 19, ddl)).contains("llegaron en Oracle Database 23ai"));
        assert!(msg(with_release_hint(
            e(),
            19,
            "CREATE VECTOR INDEX IX ON T (V)"
        ))
        .contains("23ai"));
        // 23ai itself, an unknown release, or no vector in the statement: as it was.
        assert_eq!(
            msg(with_release_hint(e(), 23, ddl)),
            "ORA-00902: invalid datatype"
        );
        assert_eq!(
            msg(with_release_hint(e(), 0, ddl)),
            "ORA-00902: invalid datatype"
        );
        assert_eq!(
            msg(with_release_hint(e(), 19, "CREATE TABLE T (V NUMBER)")),
            "ORA-00902: invalid datatype"
        );
    }

    #[test]
    fn literals_take_any_length() {
        assert_eq!(literal(&json!([1.0, 2.5])).as_deref(), Some("'[1,2.5]'"));
        assert_eq!(literal(&json!("x")), None);
        let long: Vec<f64> = (0..1536).map(|i| i as f64 * 0.001234567).collect();
        let lit = literal(&json!(long)).unwrap();
        assert!(lit.starts_with("TO_CLOB('[0,"), "{}", &lit[..40]);
        assert!(lit.contains("\n  || '"));
        // Every chunk fits a literal and the chunks put back the text.
        let text = vector::value_text(&json!(long)).unwrap();
        let mut back = String::new();
        for part in lit.split("\n  || ") {
            let inner = part
                .trim_start_matches("TO_CLOB(")
                .trim_end_matches(')')
                .trim_matches('\'');
            assert!(inner.len() <= MAX_LITERAL);
            back.push_str(inner);
        }
        assert_eq!(back, text);
    }

    #[test]
    fn long_text_literals_escape_every_chunk() {
        // Undo `text_literal`: the chunks' text, quotes single again.
        let unquote = |lit: &str| -> String {
            lit.split("\n  || ")
                .enumerate()
                .map(|(i, c)| {
                    let c = match c.strip_prefix("TO_CLOB(") {
                        Some(r) if i == 0 => r.strip_suffix(')').unwrap(),
                        _ => c,
                    };
                    let inner = c.strip_prefix('\'').unwrap().strip_suffix('\'').unwrap();
                    assert!(inner.len() <= MAX_LITERAL, "{}", inner.len());
                    assert!(!inner.replace("''", "").contains('\''), "{inner}");
                    inner.replace("''", "'")
                })
                .collect()
        };
        // A quote, in short and long text.
        let short = "x'); DROP TABLE DOCS; --";
        assert_eq!(text_literal(short), "'x''); DROP TABLE DOCS; --'");
        let long = format!("{short}{}", "a".repeat(4100));
        let lit = text_literal(&long);
        assert!(lit.starts_with("TO_CLOB('x''); DROP"), "{}", &lit[..40]);
        assert_eq!(unquote(&lit), long);
        // Quotes that push a chunk past the limit once doubled.
        let quotes = "'".repeat(4500);
        assert_eq!(unquote(&text_literal(&quotes)), quotes);
        // 3999 bytes and a two-byte character: the cut is before it, and
        // nothing is lost.
        let accent = format!("{}é{}", "a".repeat(3999), "b".repeat(10));
        assert_eq!(accent.len(), 4011);
        let lit = text_literal(&accent);
        assert!(lit.contains("\n  || 'éb"), "{}", &lit[lit.len() - 30..]);
        assert_eq!(unquote(&lit), accent);
        // Exactly the limit: one literal.
        assert_eq!(
            text_literal(&"a".repeat(MAX_LITERAL)).len(),
            MAX_LITERAL + 2
        );
    }

    #[test]
    fn typed_literals_refuse_text_that_isnt_a_vector() {
        let shape = |t: &str| Shape::parse(t).unwrap();
        let attack = json!(format!("x'); DROP TABLE DOCS; --{}", "a".repeat(4100)));
        for t in [
            "VECTOR(3, FLOAT32)",
            "VECTOR(16, BINARY)",
            "VECTOR(5, FLOAT32, SPARSE)",
            "VECTOR",
        ] {
            assert_eq!(typed_literal(&shape(t), &attack), None, "{t}");
            assert_eq!(typed_literal(&shape(t), &json!("abc")), None, "{t}");
            assert_eq!(typed_literal(&shape(t), &json!(["a'", "b"])), None, "{t}");
        }
        // Vectors that need no change, in their canonical text.
        assert_eq!(
            typed_literal(&shape("VECTOR(16, BINARY)"), &json!(" [255, 1] ")).as_deref(),
            Some("'[255,1]'")
        );
        assert_eq!(
            typed_literal(
                &shape("VECTOR(5, FLOAT32, SPARSE)"),
                &json!("[5, [1,4], [1.5, 3]]")
            )
            .as_deref(),
            Some("'[5,[1,4],[1.5,3]]'")
        );
    }

    #[test]
    fn indexes_both_ways() {
        let hnsw = Details::from_params(
            r#"{"type":"HNSW","distance":"EUCLIDEAN","accuracy":95,"efConstruction":100,"num_neighbors":16}"#,
        );
        let v = index(Some("hnsw"), &hnsw);
        assert_eq!(v.metric.as_deref(), Some("l2"));
        assert_eq!(v.params.get("m").map(String::as_str), Some("16"));
        assert_eq!(
            v.params.get("ef_construction").map(String::as_str),
            Some("100")
        );
        assert_eq!(
            index_sql("\"T\"", "\"IX\"", &["\"V\"".into()], &v).unwrap(),
            "CREATE VECTOR INDEX \"IX\" ON \"T\" (\"V\") ORGANIZATION INMEMORY NEIGHBOR GRAPH \
             DISTANCE EUCLIDEAN WITH TARGET ACCURACY 95 PARAMETERS (TYPE HNSW, NEIGHBORS 16, EFCONSTRUCTION 100)"
        );
        let ivf = Details::from_params(
            r#"{"type":"IVF_FLAT","distance":"COSINE","accuracy":90,"target_centroids":2}"#,
        );
        let v = index(Some("ivf"), &ivf);
        assert_eq!(v.params.get("lists").map(String::as_str), Some("2"));
        assert_eq!(
            index_sql("\"T\"", "\"IX\"", &["\"V\"".into(), "\"C\"".into()], &v).unwrap(),
            "CREATE VECTOR INDEX \"IX\" ON \"T\" (\"V\") INCLUDE (\"C\") ORGANIZATION NEIGHBOR PARTITIONS \
             DISTANCE COSINE WITH TARGET ACCURACY 90 PARAMETERS (TYPE IVF, NEIGHBOR PARTITIONS 2)"
        );
        // From the designer: standard names only, the rest the engine's.
        let d = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("ip".into()),
            ..Default::default()
        };
        assert_eq!(
            index_sql("\"T\"", "\"IX\"", &["\"V\"".into()], &d).unwrap(),
            "CREATE VECTOR INDEX \"IX\" ON \"T\" (\"V\") ORGANIZATION INMEMORY NEIGHBOR GRAPH DISTANCE DOT"
        );
        let bad = VectorIndex {
            algorithm: Some("diskann".into()),
            ..Default::default()
        };
        assert!(index_sql("t", "ix", &["v".into()], &bad).is_err());
        assert!(index_sql("t", "ix", &[], &d).is_err());
        assert_eq!(algorithm_of("INMEMORY_NEIGHBOR_GRAPH_HNSW"), Some("hnsw"));
        assert_eq!(algorithm_of("NEIGHBOR PARTITIONS"), Some("ivf"));
        assert_eq!(metric_of("MANHATTAN"), "l1");
        assert_eq!(native_metric("jaccard"), Some("JACCARD"));
    }
}
