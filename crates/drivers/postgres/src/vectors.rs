//! Vectors (embeddings) per variant (see `dbine_driver::vector`).
//!
//! - **pgvector** and the engines that load it or a compatible extension
//!   (PostgreSQL and its managed services, TimescaleDB with pgvectorscale,
//!   YugabyteDB, Greenplum 7 and its forks, openGauss' DataVec, EDB,
//!   Fujitsu, KingbaseES): `vector(n)`, `halfvec(n)` and `sparsevec(n)`,
//!   whose text is already a JSON array (`sparsevec` keeps its own
//!   `{1:1.5,3:2}/5`). Indexes `USING hnsw | ivfflat | diskann | scann |
//!   ybhnsw` with an operator class per distance (`vector_cosine_ops`,
//!   `halfvec_l2_ops`, `bit_hamming_ops`; AlloyDB's ScaNN: `cosine`, `l2`,
//!   `dot_product`) and their build parameters as storage parameters.
//!   `bit(n)` (binary quantization) stays PostgreSQL's bit string: only
//!   its index is a vector index (`bit_hamming_ops`, `bit_jaccard_ops`).
//! - **CockroachDB** (25.2+): `VECTOR(n)` and `CREATE VECTOR INDEX` (C-SPANN,
//!   reported as `USING cspann`), whose prefix columns come before the
//!   vector column.
//! - **CrateDB**: `FLOAT_VECTOR(n)`, indexed (HNSW) by being declared; its
//!   values travel as `real[]` (`{"1.0","2.5"}`) and are written as array
//!   literals (`[1.0, 2.5]`), never as text.
//! - **RisingWave**: `vector(n)` as pgvector writes it.
//!
//! A vector index is [`VectorIndex`] in [`IndexDef::vector`]: the access
//! method as the algorithm, the operator class as a standard metric and
//! the storage parameters as its parameters. Its [`VECTOR_OPTION`] option is
//! the index's DDL body, so comparing two indexes (which looks at the
//! options) sees a changed metric or parameter.

use crate::Variant;
use dbine_driver::vector::{metric, param};
use dbine_driver::{
    ColumnDef, IndexDef, ResultColumn, TableSchema, VectorElement, VectorIndex, VectorIndexSpec,
    VectorMeta, VectorParamSpec,
};
use std::collections::BTreeMap;

/// [`IndexDef::options`] key of a vector index: its DDL body (`USING hnsw
/// ("v" vector_cosine_ops) WITH (m=16)`), what compare and sync look at.
pub(crate) const VECTOR_OPTION: &str = "vector";

/// Variants that take pgvector (or a compatible extension) where it is
/// installed.
pub(crate) fn pgvector(v: Variant) -> bool {
    matches!(
        v,
        Variant::Postgres
            | Variant::Timescale
            | Variant::Yugabyte
            | Variant::AlloyDb
            | Variant::CloudSql
            | Variant::Aurora
            | Variant::Edb
            | Variant::Fujitsu
            | Variant::Kingbase
            | Variant::OpenGauss
            | Variant::Greenplum
            | Variant::Cloudberry
            | Variant::Greengage
    )
}

/// Variants whose engine stores vectors and that this driver handles.
pub(crate) fn supported(v: Variant) -> bool {
    pgvector(v)
        || matches!(
            v,
            Variant::Cockroach | Variant::CrateDb | Variant::RisingWave
        )
}

/// A vector type's name (lower case, without its dimension) for this
/// variant: its element and whether it is sparse.
fn kind_of(v: Variant, name: &str) -> Option<(VectorElement, bool)> {
    if !supported(v) {
        return None;
    }
    match name {
        "vector" if v != Variant::CrateDb => Some((VectorElement::F32, false)),
        "halfvec" if pgvector(v) => Some((VectorElement::F16, false)),
        "sparsevec" if pgvector(v) => Some((VectorElement::F32, true)),
        "float_vector" if v == Variant::CrateDb => Some((VectorElement::F32, false)),
        _ => None,
    }
}

/// A declared type (`vector(3)`, `halfvec(1536)`, `sparsevec(5)`,
/// `VECTOR(3)`, `FLOAT_VECTOR(3)`, `public.vector(3)`) as a vector of this
/// variant, `None` for any other type.
pub(crate) fn meta_of_type(v: Variant, data_type: &str) -> Option<VectorMeta> {
    let t = data_type.trim().to_ascii_lowercase();
    let (name, dims) = match t.split_once('(') {
        Some((n, rest)) => (n.trim(), rest.split_once(')').map(|(d, _)| d.trim())),
        None => (t.as_str(), None),
    };
    // A schema-qualified type (`extensions.vector` on Supabase).
    let name = name.rsplit('.').next().unwrap_or(name).trim_matches('"');
    let (element, sparse) = kind_of(v, name)?;
    let dims = dims.and_then(|d| d.parse::<u32>().ok());
    Some(VectorMeta {
        dims,
        element,
        sparse,
    })
}

/// A sibling's vector type in this variant's spelling, for a table that
/// came from another engine of the family (same dialect, so the type
/// arrives as the source wrote it): CockroachDB's `VECTOR(n)`, CrateDB's
/// `FLOAT_VECTOR(n)`, pgvector's `vector(n)`. `None` when it stays as it is.
pub(crate) fn sibling_type(v: Variant, data_type: &str) -> Option<String> {
    if meta_of_type(v, data_type).is_some() || !supported(v) {
        return None;
    }
    let t = data_type.trim().to_ascii_lowercase();
    let (name, dims) = match t.split_once('(') {
        Some((n, rest)) => (
            n.trim(),
            rest.split_once(')').map(|(d, _)| d.trim().to_string()),
        ),
        None => (t.as_str(), None),
    };
    let name = name.rsplit('.').next().unwrap_or(name).trim_matches('"');
    if !matches!(name, "vector" | "halfvec" | "float_vector") {
        return None;
    }
    let dims = dims.filter(|d| d.parse::<u32>().is_ok());
    Some(match (v, dims) {
        (Variant::Cockroach, Some(d)) => format!("VECTOR({d})"),
        (Variant::Cockroach, None) => "VECTOR".into(),
        (Variant::CrateDb, Some(d)) => format!("FLOAT_VECTOR({d})"),
        // CrateDB's vectors need their dimension.
        (Variant::CrateDb, None) => return None,
        (_, Some(d)) => format!("vector({d})"),
        (_, None) => "vector".into(),
    })
}

/// `t` with its siblings' vector types in this variant's spelling
/// ([`sibling_type`]).
pub(crate) fn sibling_types(v: Variant, t: &TableSchema) -> std::borrow::Cow<'_, TableSchema> {
    if !t
        .columns
        .iter()
        .any(|c| sibling_type(v, &c.data_type).is_some())
    {
        return std::borrow::Cow::Borrowed(t);
    }
    let mut out = t.clone();
    for c in &mut out.columns {
        if let Some(ty) = sibling_type(v, &c.data_type) {
            c.data_type = ty;
        }
    }
    std::borrow::Cow::Owned(out)
}

/// A result column whose type the prepared statement described
/// (`typname`, `typmod`): a vector column gets its metadata and its type
/// with the dimension (`vector(3)`), as `format_type` writes it.
pub(crate) fn mark_column(v: Variant, c: &mut ResultColumn, typname: &str, typmod: i32) {
    let Some((element, sparse)) = kind_of(v, &typname.to_ascii_lowercase()) else {
        return;
    };
    let dims = u32::try_from(typmod).ok().filter(|d| *d > 0);
    if let Some(d) = dims {
        c.type_name = format!("{typname}({d})");
    }
    c.vector = Some(VectorMeta {
        dims,
        element,
        sparse,
    });
}

/// CrateDB's text of a `FLOAT_VECTOR` (`{"1.0","2.5"}`, also without
/// quotes) as the canonical JSON array (`[1,2.5]`); `None` if it isn't one.
pub(crate) fn crate_text(s: &str) -> Option<String> {
    let inner = s.trim().strip_prefix('{')?.strip_suffix('}')?.trim();
    if inner.is_empty() {
        return Some("[]".into());
    }
    let nums: Option<Vec<f32>> = inner
        .split(',')
        .map(|n| n.trim().trim_matches('"').parse::<f32>().ok())
        .collect();
    Some(dbine_driver::vector::text(nums?))
}

/// A vector of an edit or a load as this variant's literal: `'[1,2.5]'`
/// (the server casts the string), CrateDB's array `[1,2.5]`. `None` when
/// `value` isn't an array of numbers or the variant has no vectors.
///
/// The array's own JSON text, as it came: the same value may be a `json`
/// or `jsonb` array (or a CrateDB `ARRAY(BIGINT)`), whose integers past
/// 2^53 and whose `1.0` must stay as they are, and every vector type reads
/// that text whole.
pub(crate) fn literal(v: Variant, value: &serde_json::Value) -> Option<String> {
    let serde_json::Value::Array(items) = value else {
        return None;
    };
    if !supported(v) || items.is_empty() || !items.iter().all(serde_json::Value::is_number) {
        return None;
    }
    let text = value.to_string();
    Some(if v == Variant::CrateDb {
        text
    } else {
        format!("'{text}'")
    })
}

/// A dense vector as pgvector's `sparsevec` text (`{1:1.5,3:2}/5`, positions
/// from 1); `text` already sparse, or not a vector, comes back as it is.
pub(crate) fn sparse_text(text: &str) -> std::borrow::Cow<'_, str> {
    match dbine_driver::vector::parse(text) {
        Some(values) => {
            let nz: Vec<String> = values
                .iter()
                .enumerate()
                .filter(|(_, x)| **x != 0.0)
                .map(|(i, x)| {
                    format!(
                        "{}:{}",
                        i + 1,
                        dbine_driver::vector::text([*x]).trim_matches(['[', ']'])
                    )
                })
                .collect();
            std::borrow::Cow::Owned(format!("{{{}}}/{}", nz.join(","), values.len()))
        }
        None => std::borrow::Cow::Borrowed(text),
    }
}

/// A binary vector (a JSON array of zeros and ones) as a `bit(n)` string
/// (`1011`); any other text comes back as it is.
pub(crate) fn bit_text(text: &str) -> std::borrow::Cow<'_, str> {
    match dbine_driver::vector::parse(text) {
        Some(values) if !values.is_empty() && values.iter().all(|x| *x == 0.0 || *x == 1.0) => {
            std::borrow::Cow::Owned(
                values
                    .iter()
                    .map(|x| if *x == 1.0 { '1' } else { '0' })
                    .collect(),
            )
        }
        _ => std::borrow::Cow::Borrowed(text),
    }
}

/// The one table a plain `SELECT … FROM t [WHERE …] [ORDER BY …] [LIMIT
/// …]` reads (its schema, if named, and name), as written; `None` for a
/// join, a subquery or anything else. RisingWave's results carry no column
/// types DBine can read, so its vector columns are found by their table.
pub(crate) fn single_table(sql: &str) -> Option<(Option<String>, String)> {
    let sql = sql.trim().trim_end_matches(';').trim();
    if !sql.get(..6)?.eq_ignore_ascii_case("select") {
        return None;
    }
    // ` FROM ` at depth 0, outside quotes.
    let bytes = sql.as_bytes();
    let (mut depth, mut quote, mut at) = (0i32, None::<u8>, None);
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match (quote, b) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'\'' | b'"') => quote = Some(b),
            (None, b'(') => depth += 1,
            (None, b')') => depth -= 1,
            (None, _)
                if depth == 0
                    && bytes[i].is_ascii_whitespace()
                    && sql
                        .get(i + 1..i + 6)
                        .is_some_and(|w| w.eq_ignore_ascii_case("from "))
                    && at.is_none() =>
            {
                at = Some(i + 6);
            }
            _ => {}
        }
        i += 1;
    }
    let rest = sql[at?..].trim_start();
    // An identifier, maybe quoted, maybe qualified.
    let mut parts: Vec<String> = Vec::new();
    let mut r = rest;
    loop {
        let (part, after) = if let Some(q) = r.strip_prefix('"') {
            let end = q.find('"')?;
            (q[..end].to_string(), &q[end + 1..])
        } else {
            let end = r
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(r.len());
            if end == 0 {
                return None;
            }
            (r[..end].to_ascii_lowercase(), &r[end..])
        };
        parts.push(part);
        match after.strip_prefix('.') {
            Some(more) => r = more,
            None => {
                r = after;
                break;
            }
        }
    }
    let tail = r.trim_start().to_ascii_lowercase();
    let ends = tail.is_empty()
        || ["where ", "order ", "limit ", "offset ", "fetch "]
            .iter()
            .any(|w| tail.starts_with(w));
    if !ends {
        return None;
    }
    let name = parts.pop()?;
    match parts.len() {
        0 => Some((None, name)),
        1 => Some((parts.pop(), name)),
        _ => None,
    }
}

// ------------------------------------------------------------------ indexes

/// Access methods of vector indexes: pgvector's, pgvectorscale's (and
/// Azure's pg_diskann), AlloyDB's, YugabyteDB's and CockroachDB's.
const VECTOR_AMS: &[&str] = &[
    "hnsw", "ivfflat", "diskann", "scann", "ivf", "ybhnsw", "cspann",
];

pub(crate) fn is_vector_am(am: &str) -> bool {
    VECTOR_AMS.contains(&am.to_ascii_lowercase().as_str())
}

/// The algorithms this variant creates, the default first.
fn algorithms(v: Variant) -> &'static [&'static str] {
    match v {
        Variant::Cockroach => &["cspann"],
        Variant::Yugabyte => &["ybhnsw"],
        Variant::Timescale => &["hnsw", "diskann", "ivfflat"],
        Variant::AlloyDb => &["hnsw", "scann", "ivfflat"],
        // DataVec, built in since 7.0.
        Variant::OpenGauss => &["hnsw", "ivfflat", "diskann"],
        Variant::RisingWave => &["hnsw", "flat"],
        _ if pgvector(v) => &["hnsw", "ivfflat"],
        _ => &[],
    }
}

/// A distance as an operator class's own word: pgvector's `l2`, `ip`,
/// `cosine`, `l1`, `hamming`, `jaccard`; ScaNN's `dot_product` for `ip`.
fn metric_word(alg: &str, m: &str) -> String {
    match (alg, m) {
        ("scann" | "ivf", metric::IP) => "dot_product".into(),
        _ => m.to_string(),
    }
}

/// An operator class (`vector_cosine_ops`, `bit_hamming_ops`, ScaNN's
/// `cosine`) as a standard metric, or `None` when it isn't a known one.
pub(crate) fn metric_of_opclass(op: &str) -> Option<&'static str> {
    let op = op.trim().to_ascii_lowercase();
    let word = op
        .strip_suffix("_ops")
        .map_or(op.as_str(), |w| w.rsplit_once('_').map_or(w, |(_, m)| m));
    Some(match word {
        "l2" => metric::L2,
        "ip" | "product" | "dot_product" => metric::IP,
        "cosine" => metric::COSINE,
        "l1" => metric::L1,
        "hamming" => metric::HAMMING,
        "jaccard" => metric::JACCARD,
        _ => return None,
    })
}

/// The distance an index of `alg` uses when its key names no operator
/// class: the type's default (ivfflat: `vector_l2_ops`, diskann:
/// `vector_cosine_ops`).
fn default_metric(alg: &str, prefix: &str) -> &'static str {
    match (alg, prefix) {
        (_, "bit") => metric::HAMMING,
        ("diskann", _) => metric::COSINE,
        _ => metric::L2,
    }
}

/// The type an index key's operator class is for (`vector`, `halfvec`,
/// `sparsevec`, `bit`): the cast of an expression key (`(v::halfvec(3))`,
/// `(binary_quantize(v)::bit(3))`), else the column's type.
fn key_type(key: &str, columns: &[ColumnDef]) -> &'static str {
    let k = key.to_ascii_lowercase();
    if k.starts_with('(') {
        for (cast, prefix) in [
            ("::halfvec", "halfvec"),
            ("::sparsevec", "sparsevec"),
            ("::bit", "bit"),
            ("binary_quantize(", "bit"),
            ("::vector", "vector"),
        ] {
            if k.contains(cast) {
                return prefix;
            }
        }
    }
    let col = columns.iter().find(|c| c.name == key).or_else(|| {
        // An expression over one column: that column's type.
        columns
            .iter()
            .find(|c| k.contains(&c.name.to_ascii_lowercase()))
    });
    let t = col
        .map(|c| c.data_type.to_ascii_lowercase())
        .unwrap_or_default();
    let base = t.split('(').next().unwrap_or("").trim();
    let base = base.rsplit('.').next().unwrap_or(base);
    match base {
        "halfvec" => "halfvec",
        "sparsevec" => "sparsevec",
        "bit" | "bit varying" | "varbit" => "bit",
        _ => "vector",
    }
}

/// The operator class for a key of type `prefix` in an `alg` index.
fn opclass(alg: &str, prefix: &str, m: &str) -> String {
    match alg {
        "scann" | "ivf" => metric_word(alg, m),
        _ if metric::ALL.contains(&m) => format!("{prefix}_{}_ops", metric_word(alg, m)),
        // An engine's own operator class, as it was read.
        _ => m.to_string(),
    }
}

/// Build parameters each algorithm takes, in the engine's names.
fn params_of(alg: &str) -> &'static [&'static str] {
    match alg {
        "hnsw" | "ybhnsw" => &[param::M, param::EF_CONSTRUCTION],
        "ivfflat" => &[param::LISTS],
        "diskann" => &[
            "num_neighbors",
            "search_list_size",
            "max_alpha",
            "storage_layout",
            "num_dimensions",
            "num_bits_per_dimension",
            // Azure's pg_diskann.
            "max_neighbors",
            "l_value_ib",
            "product_quantized",
        ],
        "scann" => &["num_leaves", "quantizer", "max_num_levels"],
        "ivf" => &[param::LISTS, "quantizer"],
        "cspann" => &[
            "min_partition_size",
            "max_partition_size",
            "build_beam_size",
        ],
        _ => &[],
    }
}

/// The algorithm this variant writes for `vi`'s: its own when it has it,
/// else its default (a sibling's index: CockroachDB's C-SPANN on
/// PostgreSQL, pgvector's HNSW on YugabyteDB). The flag says it changed.
fn algorithm_for(v: Variant, vi: &VectorIndex, kind: Option<&str>) -> (String, bool) {
    let offered = algorithms(v);
    let asked = vi
        .algorithm
        .as_deref()
        .or(kind)
        .map(|a| a.trim().to_ascii_lowercase())
        .filter(|a| !a.is_empty());
    let default = offered.first().copied().unwrap_or("hnsw");
    match asked {
        None => (default.to_string(), false),
        Some(a) if offered.contains(&a.as_str()) => (a, false),
        // pgvector's other access methods (or one this build doesn't know)
        // on a variant with pgvector: as they are.
        Some(a)
            if pgvector(v)
                && v != Variant::Yugabyte
                && !matches!(a.as_str(), "cspann" | "ybhnsw") =>
        {
            (a, false)
        }
        // YugabyteDB takes `ybhnsw` for pgvector's HNSW.
        Some(_) => (default.to_string(), true),
    }
}

/// A vector index read from the catalog: `am` (its access method), `def`
/// (`pg_get_indexdef`) and `reloptions` (`name=value`; CockroachDB keeps
/// them in the definition's `WITH (…)`). `None` when it isn't one.
pub(crate) fn from_catalog(
    am: &str,
    def: &str,
    reloptions: &[(String, String)],
) -> Option<VectorIndex> {
    let am = using_of(def)
        .filter(|u| is_vector_am(u))
        .unwrap_or_else(|| am.to_ascii_lowercase());
    if !is_vector_am(&am) {
        return None;
    }
    let keys = crate::compare::key_list(def)
        .map(|l| crate::compare::split_top(&l))
        .unwrap_or_default();
    // The vector key: the one with an operator class (CockroachDB's prefix
    // columns have none), else the first.
    let mut opclass_word = None;
    for k in &keys {
        if let Some(word) = trailing_word(k) {
            opclass_word = Some(word);
            break;
        }
    }
    let prefix = keys
        .iter()
        .find(|k| trailing_word(k).is_some())
        .or(keys.first())
        .map(|k| {
            let k = k.to_ascii_lowercase();
            ["halfvec", "sparsevec", "bit"]
                .into_iter()
                .find(|p| {
                    trailing_word(&k).is_some_and(|w| w.starts_with(&format!("{p}_")))
                        || k.contains(&format!("::{p}"))
                })
                .unwrap_or("vector")
        })
        .unwrap_or("vector");
    let metric = match opclass_word {
        Some(w) => metric_of_opclass(&w).map(str::to_string).unwrap_or(w),
        None => default_metric(&am, prefix).to_string(),
    };
    let mut params: BTreeMap<String, String> = reloptions.iter().cloned().collect();
    if params.is_empty() {
        params = with_options(def);
    }
    Some(VectorIndex {
        algorithm: Some(am),
        metric: Some(metric),
        params,
    })
}

/// The access method after ` USING ` in a definition.
fn using_of(def: &str) -> Option<String> {
    let at = def.find(" USING ")?;
    def[at + 7..]
        .split(|c: char| c.is_whitespace() || c == '(')
        .find(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
}

/// The operator class ending a key (`v vector_cosine_ops` → the class;
/// ScaNN's `embedding cosine`), `None` for a bare column or expression.
fn trailing_word(key: &str) -> Option<String> {
    let k = key.trim();
    if k.ends_with(')') || k.ends_with('"') {
        return None;
    }
    let (head, last) = k.rsplit_once(char::is_whitespace)?;
    if head.trim().is_empty()
        || matches!(
            last.to_ascii_uppercase().as_str(),
            "ASC" | "DESC" | "FIRST" | "LAST"
        )
    {
        return None;
    }
    Some(last.to_string())
}

/// `name=value` pairs of a definition's trailing `WITH (…)`, quotes off.
fn with_options(def: &str) -> BTreeMap<String, String> {
    let Some(at) = def.rfind(" WITH (") else {
        return BTreeMap::new();
    };
    let body = &def[at + 7..];
    let Some(end) = body.find(')') else {
        return BTreeMap::new();
    };
    crate::compare::split_top(&body[..end])
        .iter()
        .filter_map(|p| p.split_once('='))
        .map(|(k, val)| {
            (
                k.trim().to_string(),
                val.trim().trim_matches('\'').to_string(),
            )
        })
        .collect()
}

/// The vector column first, then the others (CockroachDB lists its prefix
/// columns before the vector column).
pub(crate) fn vector_first(ix: &mut IndexDef, columns: &[ColumnDef], v: Variant) {
    if ix.columns.len() < 2 {
        return;
    }
    let is_vector = |c: &String| {
        columns
            .iter()
            .find(|x| &x.name == c)
            .is_some_and(|x| meta_of_type(v, &x.data_type).is_some())
    };
    if let Some(at) = ix.columns.iter().position(is_vector).filter(|at| *at > 0) {
        let col = ix.columns.remove(at);
        ix.columns.insert(0, col);
    }
}

/// The body of a vector index's DDL after its table: `USING hnsw ("v"
/// vector_cosine_ops) WITH (m=16)`; CockroachDB's: `("uid", "v"
/// vector_cosine_ops) WITH (…)`.
pub(crate) fn body(v: Variant, t: &TableSchema, ix: &IndexDef, vi: &VectorIndex) -> String {
    let (alg, translated) = algorithm_for(v, vi, ix.kind.as_deref());
    let Some(first) = ix.columns.first() else {
        return String::new();
    };
    if v == Variant::RisingWave {
        return rw_body(&alg, translated, first, ix, vi);
    }
    let prefix = key_type(first, &t.columns);
    let m = vi
        .metric
        .as_deref()
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| default_metric(&alg, prefix).to_string());
    // A sibling's metric this algorithm can't take as a word stays standard.
    let vec_key = format!(
        "{} {}",
        crate::compare::index_column(first),
        opclass(&alg, prefix, &m)
    );
    let others: Vec<String> = ix.columns[1..]
        .iter()
        .map(|c| crate::compare::index_column(c))
        .collect();
    let keys = if v == Variant::Cockroach {
        // Prefix columns go before the vector column.
        let mut k = others;
        k.push(vec_key);
        k.join(", ")
    } else {
        std::iter::once(vec_key)
            .chain(others)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let known = params_of(&alg);
    let with: Vec<String> = vi
        .params
        .iter()
        .filter(|(k, _)| !translated || known.contains(&k.as_str()))
        .filter(|(_, val)| !val.trim().is_empty())
        .map(|(k, val)| {
            let val = val.trim();
            if val.parse::<f64>().is_ok() {
                format!("{k}={val}")
            } else {
                format!("{k}='{}'", val.replace('\'', "''"))
            }
        })
        .collect();
    let mut s = if v == Variant::Cockroach {
        format!("({keys})")
    } else {
        format!("USING {alg} ({keys})")
    };
    if !with.is_empty() {
        s.push_str(&format!(" WITH ({})", with.join(", ")));
    }
    s
}

/// RisingWave's: `USING HNSW ("v") INCLUDE ("id") WITH (distance_type =
/// 'cosine', m = 16)`, the distance being one more parameter.
fn rw_body(alg: &str, translated: bool, first: &str, ix: &IndexDef, vi: &VectorIndex) -> String {
    let m = vi
        .metric
        .as_deref()
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| metric::L2.to_string());
    let distance = match m.as_str() {
        metric::IP => "inner_product".to_string(),
        other => other.to_string(),
    };
    let known = params_of(alg);
    let mut with = vec![format!(
        "distance_type = '{}'",
        distance.replace('\'', "''")
    )];
    with.extend(
        vi.params
            .iter()
            .filter(|(k, _)| k.as_str() != RW_DISTANCE)
            .filter(|(k, _)| !translated || known.contains(&k.as_str()))
            .filter(|(_, val)| !val.trim().is_empty())
            .map(|(k, val)| {
                let val = val.trim();
                if val.parse::<f64>().is_ok() {
                    format!("{k} = {val}")
                } else {
                    format!("{k} = '{}'", val.replace('\'', "''"))
                }
            }),
    );
    let include = if ix.include.is_empty() {
        String::new()
    } else {
        format!(
            " INCLUDE ({})",
            ix.include
                .iter()
                .map(|c| crate::compare::index_column(c))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "USING {} ({}){include} WITH ({})",
        alg.to_ascii_uppercase(),
        crate::compare::index_column(first),
        with.join(", ")
    )
}

/// RisingWave's storage parameter that holds the distance.
const RW_DISTANCE: &str = "distance_type";

/// A RisingWave index read from its `CREATE INDEX` (`pg_indexes.indexdef`):
/// `USING HNSW | FLAT (v) … WITH (distance_type = 'l2', m = 16)`. `None`
/// for any other index.
pub(crate) fn from_rw_def(def: &str) -> Option<VectorIndex> {
    let alg = using_of(def).filter(|a| matches!(a.as_str(), "hnsw" | "flat"))?;
    let mut params = with_options(def);
    let metric = params
        .remove(RW_DISTANCE)
        .map(|d| match d.trim().to_ascii_lowercase().as_str() {
            "inner_product" => metric::IP.to_string(),
            other => other.to_string(),
        });
    Some(VectorIndex {
        algorithm: Some(alg),
        metric,
        params,
    })
}

/// `CREATE [VECTOR] INDEX` for a vector index of `t` (`table`: qualified
/// and quoted).
pub(crate) fn index_ddl(
    v: Variant,
    table: &str,
    t: &TableSchema,
    ix: &IndexDef,
    vi: &VectorIndex,
    if_exists: bool,
) -> String {
    let q = |s: &str| dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Double, s);
    let what = if v == Variant::Cockroach {
        "VECTOR INDEX"
    } else {
        "INDEX"
    };
    let mut s = format!(
        "CREATE {}{what} {}{} ON {table} {}",
        if ix.unique && v != Variant::Cockroach {
            "UNIQUE "
        } else {
            ""
        },
        if if_exists { "IF NOT EXISTS " } else { "" },
        q(&ix.name),
        body(v, t, ix, vi),
    );
    if let Some(w) = ix.filter.as_deref().filter(|w| !w.trim().is_empty()) {
        s.push_str(&format!(" WHERE {w}"));
    }
    s.push(';');
    s
}

/// A table's vector indexes as read: the vector column first and the
/// [`VECTOR_OPTION`] ([`canonical_options`]).
pub(crate) fn finish_read(v: Variant, t: &mut TableSchema) {
    if t.indexes.iter().all(|ix| ix.vector.is_none()) {
        return;
    }
    let columns = t.columns.clone();
    for ix in t.indexes.iter_mut().filter(|ix| ix.vector.is_some()) {
        vector_first(ix, &columns, v);
    }
    canonical_options(v, t);
}

/// Every vector index of `t` with its [`VECTOR_OPTION`] for comparing: the
/// DDL body, in place of the key list and the storage parameters it
/// already says.
pub(crate) fn canonical_options(v: Variant, t: &mut TableSchema) {
    let snapshot = t.clone();
    for ix in t.indexes.iter_mut() {
        let Some(vi) = ix.vector.clone() else {
            continue;
        };
        ix.options.remove(crate::compare::KEYS);
        for k in vi.params.keys() {
            ix.options.remove(k);
        }
        ix.options
            .insert(VECTOR_OPTION.into(), body(v, &snapshot, ix, &vi));
    }
}

// ------------------------------------------------------------------ designer

/// The vector types the designer offers, with an example dimension.
pub(crate) fn data_types(v: Variant) -> &'static [&'static str] {
    match v {
        Variant::Cockroach => &["VECTOR(1536)"],
        Variant::CrateDb => &[],
        Variant::RisingWave => &["vector(1536)"],
        _ if pgvector(v) => &[
            "vector(1536)",
            "halfvec(1536)",
            "sparsevec(1536)",
            "bit(1536)",
        ],
        _ => &[],
    }
}

/// The vector indexes the designer makes on this variant. CrateDB has none
/// to make: a `FLOAT_VECTOR` column is indexed (HNSW) by being declared.
pub(crate) fn designer_spec(v: Variant) -> Option<VectorIndexSpec> {
    let algs = algorithms(v);
    if algs.is_empty() {
        return None;
    }
    let p =
        |key: &'static str, algorithms: Vec<&'static str>, default: &'static str| VectorParamSpec {
            key,
            algorithms,
            default,
        };
    let mut params = Vec::new();
    let metrics: Vec<&'static str> = match v {
        Variant::Cockroach => {
            params.push(p("min_partition_size", vec![], "16"));
            params.push(p("max_partition_size", vec![], "128"));
            params.push(p("build_beam_size", vec![], "8"));
            vec![metric::L2, metric::COSINE, metric::IP]
        }
        Variant::Yugabyte => {
            params.push(p(param::M, vec![], "16"));
            params.push(p(param::EF_CONSTRUCTION, vec![], "64"));
            vec![metric::L2, metric::COSINE, metric::IP]
        }
        // On append-only tables only.
        Variant::RisingWave => {
            params.push(p(param::M, vec!["hnsw"], "16"));
            params.push(p(param::EF_CONSTRUCTION, vec!["hnsw"], ""));
            vec![metric::COSINE, metric::L2, metric::IP, metric::L1]
        }
        _ => {
            params.push(p(param::M, vec!["hnsw"], "16"));
            params.push(p(param::EF_CONSTRUCTION, vec!["hnsw"], "64"));
            params.push(p(param::LISTS, vec!["ivfflat"], "100"));
            // pgvectorscale's (openGauss' DiskANN takes its defaults).
            if v == Variant::Timescale {
                params.push(p("num_neighbors", vec!["diskann"], "50"));
                params.push(p("search_list_size", vec!["diskann"], "100"));
                params.push(p("max_alpha", vec!["diskann"], "1.2"));
                params.push(p("storage_layout", vec!["diskann"], "memory_optimized"));
            }
            if algs.contains(&"scann") {
                params.push(p("num_leaves", vec!["scann"], ""));
                params.push(p("quantizer", vec!["scann"], "SQ8"));
            }
            vec![
                metric::COSINE,
                metric::L2,
                metric::IP,
                metric::L1,
                metric::HAMMING,
                metric::JACCARD,
            ]
        }
    };
    Some(VectorIndexSpec {
        algorithms: algs.to_vec(),
        metrics,
        params,
        filter_columns: v == Variant::Cockroach,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            ..Default::default()
        }
    }

    fn table() -> TableSchema {
        TableSchema {
            schema: Some("public".into()),
            name: "items".into(),
            columns: vec![
                col("id", "integer"),
                col("uid", "bigint"),
                col("v", "vector(3)"),
                col("h", "halfvec(3)"),
                col("s", "sparsevec(5)"),
                col("b", "bit(4)"),
            ],
            ..Default::default()
        }
    }

    fn vix(alg: Option<&str>, m: Option<&str>, params: &[(&str, &str)]) -> VectorIndex {
        VectorIndex {
            algorithm: alg.map(Into::into),
            metric: m.map(Into::into),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn ix(name: &str, cols: &[&str], vi: VectorIndex) -> IndexDef {
        IndexDef {
            name: name.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            vector: Some(vi),
            ..Default::default()
        }
    }

    #[test]
    fn types_by_variant() {
        let m = |v, t| meta_of_type(v, t);
        assert_eq!(
            m(Variant::Postgres, "vector(3)"),
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(
            m(Variant::Postgres, "halfvec(1536)"),
            Some(VectorMeta::dense(Some(1536), VectorElement::F16))
        );
        assert_eq!(
            m(Variant::Timescale, "sparsevec(5)"),
            Some(VectorMeta::sparse(Some(5), VectorElement::F32))
        );
        assert_eq!(
            m(Variant::Postgres, "extensions.vector"),
            Some(VectorMeta::dense(None, VectorElement::F32))
        );
        assert_eq!(
            m(Variant::Cockroach, "VECTOR(3)"),
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(m(Variant::Cockroach, "halfvec(3)"), None);
        assert_eq!(
            m(Variant::CrateDb, "FLOAT_VECTOR(3)"),
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(m(Variant::CrateDb, "vector(3)"), None);
        assert_eq!(m(Variant::Postgres, "bit(4)"), None);
        assert_eq!(m(Variant::Postgres, "integer[]"), None);
        assert_eq!(m(Variant::Redshift, "vector(3)"), None);
        assert_eq!(m(Variant::Materialize, "vector(3)"), None);
    }

    #[test]
    fn siblings_vector_types() {
        assert_eq!(
            sibling_type(Variant::Cockroach, "halfvec(3)").as_deref(),
            Some("VECTOR(3)")
        );
        assert_eq!(
            sibling_type(Variant::Cockroach, "FLOAT_VECTOR(3)").as_deref(),
            Some("VECTOR(3)")
        );
        assert_eq!(sibling_type(Variant::Cockroach, "vector(3)"), None);
        assert_eq!(sibling_type(Variant::Cockroach, "sparsevec(3)"), None);
        assert_eq!(
            sibling_type(Variant::CrateDb, "vector(3)").as_deref(),
            Some("FLOAT_VECTOR(3)")
        );
        assert_eq!(sibling_type(Variant::CrateDb, "VECTOR"), None);
        assert_eq!(
            sibling_type(Variant::Postgres, "FLOAT_VECTOR(3)").as_deref(),
            Some("vector(3)")
        );
        assert_eq!(sibling_type(Variant::Postgres, "halfvec(3)"), None);
        assert_eq!(
            sibling_type(Variant::RisingWave, "halfvec(3)").as_deref(),
            Some("vector(3)")
        );
        assert_eq!(sibling_type(Variant::Redshift, "vector(3)"), None);
        assert_eq!(sibling_type(Variant::Postgres, "integer"), None);
    }

    #[test]
    fn result_columns_get_dims_from_the_type_modifier() {
        let mut c = ResultColumn::new("v", "vector");
        mark_column(Variant::Postgres, &mut c, "vector", 3);
        assert_eq!(c.type_name, "vector(3)");
        assert_eq!(
            c.vector,
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        let mut c = ResultColumn::new("v", "vector");
        mark_column(Variant::Postgres, &mut c, "vector", -1);
        assert_eq!(c.type_name, "vector");
        assert_eq!(c.vector, Some(VectorMeta::dense(None, VectorElement::F32)));
        let mut c = ResultColumn::new("s", "sparsevec");
        mark_column(Variant::Postgres, &mut c, "sparsevec", 5);
        assert!(c.vector.as_ref().unwrap().sparse);
        let mut c = ResultColumn::new("t", "text");
        mark_column(Variant::Postgres, &mut c, "text", -1);
        assert_eq!(c.vector, None);
    }

    #[test]
    fn crate_text_becomes_a_json_array() {
        assert_eq!(
            crate_text(r#"{"1.0","2.5","-3.0"}"#).as_deref(),
            Some("[1,2.5,-3]")
        );
        assert_eq!(crate_text("{1.5,2}").as_deref(), Some("[1.5,2]"));
        assert_eq!(crate_text("{}").as_deref(), Some("[]"));
        // Float32: the shortest text that reads back as the same float.
        assert_eq!(
            crate_text(r#"{"0.1","0.30000001"}"#).as_deref(),
            Some("[0.1,0.3]")
        );
        assert_eq!(crate_text("[1,2]"), None);
        assert_eq!(crate_text(r#"{"a"}"#), None);
    }

    #[test]
    fn literals() {
        assert_eq!(
            literal(Variant::Postgres, &json!([1.0, 2.5, -3])).as_deref(),
            Some("'[1.0,2.5,-3]'")
        );
        assert_eq!(
            literal(Variant::Cockroach, &json!([1, 2])).as_deref(),
            Some("'[1,2]'")
        );
        assert_eq!(
            literal(Variant::CrateDb, &json!([1.5, 2])).as_deref(),
            Some("[1.5,2]")
        );
        assert_eq!(literal(Variant::Postgres, &json!(["a"])), None);
        assert_eq!(literal(Variant::Postgres, &json!([])), None);
        assert_eq!(literal(Variant::Postgres, &json!("[1,2]")), None);
        // Variants without vectors keep their own array literal.
        for v in [
            Variant::Redshift,
            Variant::Materialize,
            Variant::H2,
            Variant::Yellowbrick,
            Variant::Denodo,
        ] {
            assert_eq!(literal(v, &json!([1, 2])), None, "{v:?}");
        }
    }

    /// The array's text is kept as it came: a `json` / `jsonb` (or CrateDB
    /// `ARRAY(BIGINT)`) value of big integers or `1.0` is not a vector to
    /// round through floats.
    #[test]
    fn literals_keep_the_exact_numbers() {
        let big = json!([9007199254740993u64, 1, 1.0, -0.000001]);
        for v in Variant::ALL.into_iter().filter(|v| supported(*v)) {
            let lit = literal(v, &big).unwrap();
            assert!(
                lit.contains("[9007199254740993,1,1.0,-1e-6]"),
                "{v:?}: {lit}"
            );
        }
    }

    #[test]
    fn dense_text_into_sparsevec_and_bits() {
        assert_eq!(sparse_text("[0,1.5,0,2,0]"), "{2:1.5,4:2}/5");
        assert_eq!(sparse_text("[0,0]"), "{}/2");
        assert_eq!(sparse_text("{1:1}/3"), "{1:1}/3");
        assert_eq!(bit_text("[1,0,1,1]"), "1011");
        assert_eq!(bit_text("1011"), "1011");
        assert_eq!(bit_text("[1,2]"), "[1,2]");
    }

    #[test]
    fn opclasses_map_to_standard_metrics() {
        for (op, m) in [
            ("vector_cosine_ops", metric::COSINE),
            ("halfvec_l2_ops", metric::L2),
            ("sparsevec_ip_ops", metric::IP),
            ("vector_l1_ops", metric::L1),
            ("bit_hamming_ops", metric::HAMMING),
            ("bit_jaccard_ops", metric::JACCARD),
            ("cosine", metric::COSINE),
            ("dot_product", metric::IP),
            ("l2", metric::L2),
        ] {
            assert_eq!(metric_of_opclass(op), Some(m), "{op}");
        }
        assert_eq!(metric_of_opclass("text_pattern_ops"), None);
        // And back, per key type and algorithm.
        assert_eq!(opclass("hnsw", "vector", "cosine"), "vector_cosine_ops");
        assert_eq!(opclass("hnsw", "halfvec", "l1"), "halfvec_l1_ops");
        assert_eq!(opclass("hnsw", "bit", "jaccard"), "bit_jaccard_ops");
        assert_eq!(opclass("scann", "vector", "ip"), "dot_product");
        assert_eq!(opclass("scann", "vector", "cosine"), "cosine");
        assert_eq!(opclass("hnsw", "vector", "my_ops"), "my_ops");
    }

    #[test]
    fn indexes_read_from_the_catalog() {
        let r = |k: &str, v: &str| (k.to_string(), v.to_string());
        let hnsw = from_catalog(
            "hnsw",
            "CREATE INDEX t_hnsw ON public.t USING hnsw (v vector_cosine_ops) WITH (m='16', ef_construction='64')",
            &[r("m", "16"), r("ef_construction", "64")],
        )
        .unwrap();
        assert_eq!(
            hnsw,
            vix(
                Some("hnsw"),
                Some("cosine"),
                &[("m", "16"), ("ef_construction", "64")]
            )
        );
        let ivf = from_catalog(
            "ivfflat",
            "CREATE INDEX t_ivf ON public.t USING ivfflat (h halfvec_l2_ops) WITH (lists=10)",
            &[r("lists", "10")],
        )
        .unwrap();
        assert_eq!(ivf, vix(Some("ivfflat"), Some("l2"), &[("lists", "10")]));
        // The type's default operator class isn't written.
        let plain = from_catalog(
            "ivfflat",
            "CREATE INDEX i ON public.t USING ivfflat (v)",
            &[],
        );
        assert_eq!(plain.unwrap().metric.as_deref(), Some("l2"));
        let disk = from_catalog(
            "diskann",
            "CREATE INDEX i ON public.t USING diskann (v)",
            &[],
        );
        assert_eq!(disk.unwrap().metric.as_deref(), Some("cosine"));
        let bits = from_catalog(
            "hnsw",
            "CREATE INDEX i ON public.t USING hnsw (b bit_hamming_ops)",
            &[],
        );
        assert_eq!(bits.unwrap().metric.as_deref(), Some("hamming"));
        let expr = from_catalog(
            "hnsw",
            "CREATE INDEX i ON public.t USING hnsw (((v)::halfvec(3)) halfvec_cosine_ops)",
            &[],
        );
        assert_eq!(expr.unwrap().metric.as_deref(), Some("cosine"));
        let scann = from_catalog(
            "scann",
            "CREATE INDEX i ON public.t USING scann (v dot_product) WITH (num_leaves='5', quantizer='SQ8')",
            &[r("num_leaves", "5"), r("quantizer", "SQ8")],
        )
        .unwrap();
        assert_eq!(scann.metric.as_deref(), Some("ip"));
        // CockroachDB: pg_am says `prefix`; the definition says cspann, and
        // its storage parameters are only in it.
        let crdb = from_catalog(
            "prefix",
            "CREATE INDEX t_v2 ON public.t USING cspann (uid, v vector_l2_ops) WITH (build_beam_size=8, min_partition_size=16, max_partition_size=128)",
            &[],
        )
        .unwrap();
        assert_eq!(
            crdb,
            vix(
                Some("cspann"),
                Some("l2"),
                &[
                    ("build_beam_size", "8"),
                    ("min_partition_size", "16"),
                    ("max_partition_size", "128")
                ]
            )
        );
        assert_eq!(
            from_catalog(
                "btree",
                "CREATE INDEX i ON public.t USING btree (a DESC)",
                &[]
            ),
            None
        );
        assert_eq!(
            from_catalog(
                "prefix",
                "CREATE INDEX i ON public.t USING btree (a ASC)",
                &[]
            ),
            None
        );
    }

    #[test]
    fn ddl_per_variant() {
        let t = table();
        let tq = "\"public\".\"items\"";
        let i = ix(
            "ix_v",
            &["v"],
            vix(
                Some("hnsw"),
                Some("cosine"),
                &[("m", "16"), ("ef_construction", "64")],
            ),
        );
        assert_eq!(
            index_ddl(Variant::Postgres, tq, &t, &i, i.vector.as_ref().unwrap(), false),
            "CREATE INDEX \"ix_v\" ON \"public\".\"items\" USING hnsw (\"v\" vector_cosine_ops) WITH (ef_construction=64, m=16);"
        );
        // halfvec, sparsevec and bit columns get their own classes.
        for (c, m, op) in [
            ("h", "l2", "halfvec_l2_ops"),
            ("s", "ip", "sparsevec_ip_ops"),
            ("b", "hamming", "bit_hamming_ops"),
        ] {
            let i = ix("i", &[c], vix(None, Some(m), &[]));
            let s = index_ddl(
                Variant::Postgres,
                tq,
                &t,
                &i,
                i.vector.as_ref().unwrap(),
                true,
            );
            assert_eq!(
                s,
                format!("CREATE INDEX IF NOT EXISTS \"i\" ON {tq} USING hnsw (\"{c}\" {op});")
            );
        }
        // Defaults: the first algorithm, L2 (Hamming on bits).
        let i = ix("i", &["v"], vix(None, None, &[]));
        assert!(index_ddl(
            Variant::Postgres,
            tq,
            &t,
            &i,
            i.vector.as_ref().unwrap(),
            false
        )
        .contains("USING hnsw (\"v\" vector_l2_ops)"));
        let i = ix("i", &["b"], vix(Some("ivfflat"), None, &[("lists", "10")]));
        assert!(index_ddl(
            Variant::Postgres,
            tq,
            &t,
            &i,
            i.vector.as_ref().unwrap(),
            false
        )
        .contains("USING ivfflat (\"b\" bit_hamming_ops) WITH (lists=10)"));
        // An expression key keeps its cast's class.
        let i = ix(
            "i",
            &["((v)::halfvec(3))"],
            vix(Some("hnsw"), Some("cosine"), &[]),
        );
        assert!(index_ddl(
            Variant::Postgres,
            tq,
            &t,
            &i,
            i.vector.as_ref().unwrap(),
            false
        )
        .contains("USING hnsw (((v)::halfvec(3)) halfvec_cosine_ops)"));
        // A partial index keeps its WHERE.
        let mut i = ix("i", &["v"], vix(Some("hnsw"), Some("l2"), &[]));
        i.filter = Some("(uid = 1)".into());
        assert!(index_ddl(
            Variant::Postgres,
            tq,
            &t,
            &i,
            i.vector.as_ref().unwrap(),
            false
        )
        .ends_with("vector_l2_ops) WHERE (uid = 1);"));
        // pgvectorscale and AlloyDB.
        let i = ix(
            "i",
            &["v"],
            vix(Some("diskann"), Some("cosine"), &[("num_neighbors", "50")]),
        );
        assert!(index_ddl(
            Variant::Timescale,
            tq,
            &t,
            &i,
            i.vector.as_ref().unwrap(),
            false
        )
        .contains("USING diskann (\"v\" vector_cosine_ops) WITH (num_neighbors=50)"));
        let i = ix(
            "i",
            &["v"],
            vix(Some("scann"), Some("ip"), &[("num_leaves", "5")]),
        );
        assert!(index_ddl(
            Variant::AlloyDb,
            tq,
            &t,
            &i,
            i.vector.as_ref().unwrap(),
            false
        )
        .contains("USING scann (\"v\" dot_product) WITH (num_leaves=5)"));
        // YugabyteDB: ybhnsw, also for pgvector's hnsw.
        let i = ix(
            "i",
            &["v"],
            vix(Some("hnsw"), Some("cosine"), &[("m", "16")]),
        );
        assert!(index_ddl(
            Variant::Yugabyte,
            tq,
            &t,
            &i,
            i.vector.as_ref().unwrap(),
            false
        )
        .contains("USING ybhnsw (\"v\" vector_cosine_ops) WITH (m=16)"));
        // CockroachDB: CREATE VECTOR INDEX, prefix columns first, its own
        // parameters only.
        let i = ix(
            "i",
            &["v", "uid"],
            vix(Some("hnsw"), Some("cosine"), &[("m", "16")]),
        );
        assert_eq!(
            index_ddl(
                Variant::Cockroach,
                tq,
                &t,
                &i,
                i.vector.as_ref().unwrap(),
                false
            ),
            "CREATE VECTOR INDEX \"i\" ON \"public\".\"items\" (\"uid\", \"v\" vector_cosine_ops);"
        );
        let i = ix(
            "i",
            &["v"],
            vix(Some("cspann"), None, &[("min_partition_size", "16")]),
        );
        assert_eq!(
            index_ddl(Variant::Cockroach, tq, &t, &i, i.vector.as_ref().unwrap(), true),
            "CREATE VECTOR INDEX IF NOT EXISTS \"i\" ON \"public\".\"items\" (\"v\" vector_l2_ops) WITH (min_partition_size=16);"
        );
        // CockroachDB's C-SPANN on PostgreSQL: pgvector's HNSW, without
        // C-SPANN's parameters.
        assert_eq!(
            index_ddl(
                Variant::Postgres,
                tq,
                &t,
                &i,
                i.vector.as_ref().unwrap(),
                false
            ),
            "CREATE INDEX \"i\" ON \"public\".\"items\" USING hnsw (\"v\" vector_l2_ops);"
        );
    }

    #[test]
    fn the_table_a_select_reads() {
        let t = |s: &str| single_table(s);
        assert_eq!(
            t("SELECT * FROM \"public\".\"items\" LIMIT 100"),
            Some((Some("public".into()), "items".into()))
        );
        assert_eq!(
            t("select id, v from items where id = 1 order by id;"),
            Some((None, "items".into()))
        );
        assert_eq!(
            t("SELECT v FROM s.Items"),
            Some((Some("s".into()), "items".into()))
        );
        assert_eq!(t("SELECT * FROM a JOIN b ON a.id = b.id"), None);
        assert_eq!(t("SELECT * FROM a, b"), None);
        assert_eq!(
            t("SELECT (SELECT 1 FROM x) FROM y"),
            Some((None, "y".into()))
        );
        assert_eq!(t("SELECT * FROM (SELECT 1) x"), None);
        assert_eq!(t("SELECT 1"), None);
        assert_eq!(t("INSERT INTO t SELECT * FROM u"), None);
    }

    #[test]
    fn risingwave_indexes() {
        let def = "CREATE INDEX ix ON t USING HNSW (v) INCLUDE(id) WITH (distance_type = 'inner_product', m = 32, ef_construction = 100)";
        let vi = from_rw_def(def).unwrap();
        assert_eq!(
            vi,
            vix(
                Some("hnsw"),
                Some("ip"),
                &[("m", "32"), ("ef_construction", "100")]
            )
        );
        let flat =
            from_rw_def("CREATE INDEX f ON t USING FLAT (v) WITH (distance_type = 'cosine')");
        assert_eq!(flat, Some(vix(Some("flat"), Some("cosine"), &[])));
        assert_eq!(from_rw_def("CREATE INDEX i ON t(name)"), None);
        let t = table();
        let mut i = ix("ix", &["v"], vi);
        i.include = vec!["id".into()];
        assert_eq!(
            index_ddl(Variant::RisingWave, "\"t\"", &t, &i, i.vector.as_ref().unwrap(), false),
            "CREATE INDEX \"ix\" ON \"t\" USING HNSW (\"v\") INCLUDE (\"id\") WITH (distance_type = 'inner_product', ef_construction = 100, m = 32);"
        );
        // pgvector's IVFFlat: RisingWave's HNSW, without `lists`.
        let i = ix("ix", &["v"], vix(Some("ivfflat"), None, &[("lists", "10")]));
        assert_eq!(
            index_ddl(
                Variant::RisingWave,
                "\"t\"",
                &t,
                &i,
                i.vector.as_ref().unwrap(),
                false
            ),
            "CREATE INDEX \"ix\" ON \"t\" USING HNSW (\"v\") WITH (distance_type = 'l2');"
        );
        let spec = designer_spec(Variant::RisingWave).unwrap();
        assert_eq!(spec.algorithms, ["hnsw", "flat"]);
    }

    #[test]
    fn the_vector_option_says_what_differs() {
        let mut a = table();
        a.indexes.push(ix(
            "i",
            &["v"],
            vix(Some("hnsw"), Some("cosine"), &[("m", "16")]),
        ));
        // As read: the key list and the storage parameters are in it.
        a.indexes[0]
            .options
            .insert(crate::compare::KEYS.into(), "v vector_cosine_ops".into());
        a.indexes[0].options.insert("m".into(), "16".into());
        let mut b = a.clone();
        b.indexes[0].vector.as_mut().unwrap().metric = Some("l2".into());
        canonical_options(Variant::Postgres, &mut a);
        canonical_options(Variant::Postgres, &mut b);
        assert_eq!(a.indexes[0].options.len(), 1);
        assert_eq!(
            a.indexes[0].options[VECTOR_OPTION],
            "USING hnsw (\"v\" vector_cosine_ops) WITH (m=16)"
        );
        assert_ne!(a.indexes[0].options, b.indexes[0].options);
    }

    #[test]
    fn cockroach_lists_the_vector_column_first() {
        let t = table();
        let mut i = ix("i", &["uid", "v"], vix(Some("cspann"), None, &[]));
        vector_first(&mut i, &t.columns, Variant::Cockroach);
        assert_eq!(i.columns, ["v", "uid"]);
    }

    #[test]
    fn designer_choices() {
        let pg = designer_spec(Variant::Postgres).unwrap();
        assert_eq!(pg.algorithms, ["hnsw", "ivfflat"]);
        assert_eq!(pg.metrics[0], metric::COSINE);
        assert!(pg.params.iter().any(|p| p.key == "m" && p.default == "16"));
        assert!(!pg.filter_columns);
        assert!(designer_spec(Variant::Timescale)
            .unwrap()
            .algorithms
            .contains(&"diskann"));
        let og = designer_spec(Variant::OpenGauss).unwrap();
        assert!(og.algorithms.contains(&"diskann"));
        assert!(!og.params.iter().any(|p| p.key == "num_neighbors"));
        assert!(designer_spec(Variant::AlloyDb)
            .unwrap()
            .algorithms
            .contains(&"scann"));
        assert_eq!(
            designer_spec(Variant::Yugabyte).unwrap().algorithms,
            ["ybhnsw"]
        );
        let crdb = designer_spec(Variant::Cockroach).unwrap();
        assert_eq!(crdb.algorithms, ["cspann"]);
        assert!(crdb.filter_columns);
        assert!(designer_spec(Variant::CrateDb).is_none());
        assert!(designer_spec(Variant::Redshift).is_none());
        assert!(data_types(Variant::Postgres).contains(&"halfvec(1536)"));
        assert!(data_types(Variant::H2).is_empty());
    }
}
