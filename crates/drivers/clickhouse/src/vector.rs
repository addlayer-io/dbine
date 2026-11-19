//! Vectors in ClickHouse. There is no vector type: an embedding is an
//! `Array(Float32)` (or `Array(BFloat16)`, `Array(Float64)`), searched with
//! the `vector_similarity` skip index and the distance functions
//! (`cosineDistance`, `L2Distance`, `dotProduct`…).
//!
//! Which arrays are vectors: `Array(Float32)` and `Array(BFloat16)`, the
//! types ClickHouse's vector search documents for embeddings (BFloat16
//! exists for machine learning), in results, describes and conversions.
//! `Array(Float64)` is ClickHouse's general array of numbers, so it's a
//! vector only where a `vector_similarity` index says so (the index, not
//! the result, knows it). A ClickHouse array has no fixed length: the
//! dimension is the index's.
//!
//! The index as DBine writes it, `vector_similarity('hnsw', 'cosineDistance',
//! 3[, quantization, m, ef_construction])`, is ClickHouse 25.8's (when it
//! left beta) and later's; the `dotProduct` distance came in 26.4, and
//! `BFloat16` in 25.1. From 24.8 to 25.7 the index was experimental, took
//! no dimension (`vector_similarity('hnsw', 'L2Distance'[, quantization, m,
//! ef_construction])`) and needed `allow_experimental_vector_similarity_index`:
//! such an index is read, and written back in its own form, with that
//! setting first ([`LEGACY_SET`]); a new one needs 25.8.
//!
//! Timeplus Proton (3.0) has `array(float32)` and the distances
//! (`cosine_distance`, `l2_distance`…) but neither `bfloat16` nor a vector
//! index.

use dbine_driver::vector::{metric, param};
use dbine_driver::{VectorElement, VectorIndex, VectorMeta};

/// The index type of a vector index.
pub const KIND: &str = "vector_similarity";
/// The `vector_similarity` granularity ClickHouse uses when none is given
/// (and recommends): an index covers a whole part.
pub const DEFAULT_GRANULARITY: &str = "100000000";
/// The index's dimension, which ClickHouse requires, as a parameter.
pub const DIMENSIONS: &str = "dimensions";

/// What an index of ClickHouse 24.8–25.7 (experimental, no dimension) needs
/// to be created; an obsolete setting from 25.8 on, which servers still
/// take.
pub const LEGACY_SET: &str = "SET allow_experimental_vector_similarity_index = 1;";

/// What a server error on a statement that makes a vector index or a
/// `BFloat16` column is likely to mean: a server older than what the
/// statement needs (DBine writes the index as ClickHouse 25.8 does, with
/// the dimension; `dotProduct` came in 26.4, `BFloat16` in 25.1). `code`
/// is the error's ClickHouse code.
pub fn version_hint(stmt: &str, code: u32, message: &str) -> Option<&'static str> {
    let low = stmt.to_ascii_lowercase();
    // INCORRECT_QUERY, INCORRECT_DATA, BAD_ARGUMENTS, SYNTAX_ERROR,
    // UNKNOWN_SETTING, SUPPORT_IS_DISABLED.
    if low.contains(KIND) && [80, 117, 36, 62, 115, 344].contains(&code) {
        return Some(
            "Los índices vector_similarity, tal como los escribe DBine (con la dimensión del vector), son de ClickHouse 25.8 o posterior, y la distancia dotProduct (producto interno) de 26.4 o posterior; antes de 25.8 el índice era experimental y no llevaba dimensión.",
        );
    }
    // UNKNOWN_TYPE
    if code == 50 && low.contains("bfloat16") && message.contains("BFloat16") {
        return Some(
            "El tipo BFloat16 es de ClickHouse 25.1 o posterior (experimental desde 24.11).",
        );
    }
    None
}

/// ClickHouse's distance functions for the index and the standard names
/// (`dotProduct` from 26.4).
const METRICS: [(&str, &str); 3] = [
    ("cosineDistance", metric::COSINE),
    ("L2Distance", metric::L2),
    ("dotProduct", metric::IP),
];

/// `Nullable(LowCardinality(T))` → `t` (an array can't be either, but a
/// result type may be spelled so).
fn base(t: &str) -> String {
    crate::base_type(t)
}

/// A column of type `t` holds vectors ([`self`]): `Array(Float32)` or
/// `Array(BFloat16)` (`array(float32)` in Timeplus).
pub fn meta(t: &str) -> Option<VectorMeta> {
    let element = match base(t).replace(' ', "").as_str() {
        "array(float32)" => VectorElement::F32,
        "array(bfloat16)" => VectorElement::Bf16,
        _ => return None,
    };
    Some(VectorMeta::dense(None, element))
}

/// A vector cell as the server wrote it (`[0.1,2,1e-7]`) as DBine's vector
/// text (`[0.1,2,0.0000001]`), each number the shortest way a Float32 (or
/// BFloat16, which a Float32 holds) reads back; `None` when it isn't a
/// whole array of finite numbers.
pub fn cell_text(raw: &str) -> Option<String> {
    let n = dbine_driver::vector::parse(raw)?;
    Some(dbine_driver::vector::text(n.into_iter().map(|x| x as f32)))
}

/// The table a browse query reads, `(database, table)`: `SELECT *` from
/// one table, as `browse_query` writes it (`` `db`.`t` `` or `` `t` ``,
/// then `WHERE`, `ORDER BY`, `LIMIT`… as the grid adds them). `None` for
/// any other statement.
pub fn browse_target(stmt: &str) -> Option<(Option<String>, String)> {
    /// One identifier at the start of `s`: `` `a\`b` `` or a bare name;
    /// the rest after it.
    fn ident(s: &str) -> Option<(String, &str)> {
        if let Some(rest) = s.strip_prefix('`') {
            let mut name = String::new();
            let mut chars = rest.char_indices();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => name.push(chars.next()?.1),
                    '`' => return Some((name, &rest[i + 1..])),
                    _ => name.push(c),
                }
            }
            return None;
        }
        let end = s
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(s.len());
        (end > 0).then(|| (s[..end].to_string(), &s[end..]))
    }
    let s = stmt.trim_start();
    let head = |s: &'_ str, word: &str| -> Option<usize> {
        s.get(..word.len())
            .filter(|h| h.eq_ignore_ascii_case(word))
            .map(|_| word.len())
    };
    let s = &s[head(s, "select")?..];
    let s = s.trim_start().strip_prefix('*')?;
    let s = s.trim_start();
    let s = &s[head(s, "from")?..];
    if !s.starts_with(char::is_whitespace) {
        return None;
    }
    let (first, rest) = ident(s.trim_start())?;
    let (db, table, rest) = match rest.strip_prefix('.') {
        Some(r) => {
            let (t, rest) = ident(r)?;
            (Some(first), t, rest)
        }
        None => (None, first, rest),
    };
    // Only the clauses the grid adds: a JOIN, a comma or an alias reads
    // more than this table.
    let rest = rest.trim_start();
    let word = rest
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let clause = matches!(
        word.as_str(),
        "WHERE" | "PREWHERE" | "ORDER" | "LIMIT" | "OFFSET" | "SETTINGS" | "FORMAT" | "FINAL"
    );
    (rest.is_empty() || rest.starts_with(';') || clause).then_some((db, table))
}

/// The vector indexes of a table, for [`indexed_arrays`]: the column (its
/// expression) and the index's whole type.
pub const INDEXED_ARRAYS_SQL: &str = "SELECT expr, type_full FROM system.data_skipping_indices
 WHERE database = {db:String} AND table = {t:String} AND type = 'vector_similarity'";

/// The columns a table's `vector_similarity` indexes search, with the
/// dimension the index states (24.8–25.7 ones state none), from the rows
/// of [`INDEXED_ARRAYS_SQL`].
pub fn indexed_arrays(rows: &[(String, String)]) -> Vec<(String, Option<u32>)> {
    rows.iter()
        .map(|(expr, ty)| {
            let col = expr.trim();
            let col = col
                .strip_prefix('`')
                .and_then(|c| c.strip_suffix('`'))
                .map_or_else(
                    || col.to_string(),
                    |c| c.replace("\\`", "`").replace("\\\\", "\\"),
                );
            let dims =
                parse_index(ty).and_then(|v| v.params.get(DIMENSIONS).and_then(|d| d.parse().ok()));
            (col, dims)
        })
        .collect()
}

/// `Array(Float64)` is ClickHouse's general array of numbers, a vector only
/// where a vector index searches it: in a browse of its table, such a
/// column is marked (`F64`, the index's dimension) and its cells become
/// DBine's vector text (each number the shortest way a Float64 reads back).
/// `indexed`: [`indexed_arrays`] of the table.
pub fn mark_indexed_arrays(
    result: &mut dbine_driver::StatementResult,
    indexed: &[(String, Option<u32>)],
) {
    for (i, c) in result.columns.iter_mut().enumerate() {
        if c.vector.is_some() || base(&c.type_name).replace(' ', "") != "array(float64)" {
            continue;
        }
        let Some((_, dims)) = indexed.iter().find(|(n, _)| *n == c.name) else {
            continue;
        };
        c.vector = Some(VectorMeta::dense(*dims, VectorElement::F64));
        for row in &mut result.rows {
            if let Some(cell) = row.get_mut(i) {
                let text = cell
                    .as_str()
                    .and_then(dbine_driver::vector::parse)
                    .map(dbine_driver::vector::text);
                if let Some(t) = text {
                    *cell = serde_json::Value::String(t);
                }
            }
        }
    }
}

/// A vector value of an edit or insert script (a JSON array of numbers) as
/// ClickHouse's array literal `[1,2.5]`.
pub fn literal(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Array(_) => dbine_driver::vector::value_text(v),
        _ => None,
    }
}

/// The quoted or bare arguments of `vector_similarity(…)`.
fn args(type_full: &str) -> Option<Vec<String>> {
    let t = type_full.trim();
    let head = t.get(..KIND.len())?;
    if !head.eq_ignore_ascii_case(KIND) {
        return None;
    }
    let inner = t[KIND.len()..]
        .trim()
        .strip_prefix('(')?
        .strip_suffix(')')?;
    Some(
        crate::schema::split_top(inner)
            .into_iter()
            .map(|a| {
                let a = a.trim();
                match a.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
                    Some(q) => q.replace("\\'", "'").replace("''", "'"),
                    None => a.to_string(),
                }
            })
            .collect(),
    )
}

/// A `vector_similarity('hnsw', 'cosineDistance', 3[, 'bf16', 32, 128])`
/// index type as its [`VectorIndex`]: the dimension as `dimensions`, the
/// quantization, `m` (`hnsw_max_connections_per_layer`) and
/// `ef_construction` (`hnsw_candidate_list_size_for_construction`); a 0
/// (ClickHouse's default) is left out. The form of 24.8–25.7, without the
/// dimension (`vector_similarity('hnsw', 'L2Distance'[, 'bf16', 32, 128])`,
/// and 24.8's six arguments with `ef_search` last), reads the same way,
/// without `dimensions`.
pub fn parse_index(type_full: &str) -> Option<VectorIndex> {
    let a = args(type_full)?;
    // The third argument is the dimension (a number) from 25.8 on, the
    // quantization (a name) before.
    let legacy = a.get(2).is_some_and(|x| x.parse::<u64>().is_err());
    let at: &[(usize, &str)] = if legacy {
        &[
            (2, param::QUANTIZATION),
            (3, param::M),
            (4, param::EF_CONSTRUCTION),
            (5, "ef_search"),
        ]
    } else {
        &[
            (2, DIMENSIONS),
            (3, param::QUANTIZATION),
            (4, param::M),
            (5, param::EF_CONSTRUCTION),
        ]
    };
    let mut v = VectorIndex {
        algorithm: a.first().map(|x| x.to_ascii_lowercase()),
        metric: a.get(1).map(|m| {
            METRICS
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(m))
                .map_or_else(|| m.clone(), |(_, std)| std.to_string())
        }),
        ..Default::default()
    };
    for &(i, key) in at {
        if let Some(x) = a.get(i).filter(|x| !x.is_empty() && x.as_str() != "0") {
            v.params.insert(key.to_string(), x.clone());
        }
    }
    Some(v)
}

/// The index type for `v`: `vector_similarity('hnsw', 'cosineDistance', 3)`,
/// with the six arguments when a quantization, `m` or `ef_construction` is
/// set (0 for the ones that aren't: ClickHouse's default). `Err`: it has no
/// dimension, which ClickHouse requires.
pub fn index_type(v: &VectorIndex) -> Result<String, String> {
    let p = |k: &str| v.params.get(k).map(|x| x.trim()).filter(|x| !x.is_empty());
    let dims = p(DIMENSIONS).ok_or(
        "ClickHouse necesita la dimensión del vector (parámetro «dimensions») para crear el índice",
    )?;
    if dims.parse::<u32>().is_err() {
        return Err(format!(
            "la dimensión del índice vectorial no es un número: «{dims}»"
        ));
    }
    let lit = |s: &str| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"));
    let algorithm = v
        .algorithm
        .as_deref()
        .filter(|a| !a.is_empty())
        .unwrap_or("hnsw");
    let m = v
        .metric
        .as_deref()
        .filter(|m| !m.is_empty())
        .unwrap_or(metric::L2);
    let native = METRICS
        .iter()
        .find(|(_, std)| *std == m)
        .map_or(m, |(n, _)| n);
    let mut out = format!("{KIND}({}, {}, {dims}", lit(algorithm), lit(native));
    let (q, mm, ef) = (
        p(param::QUANTIZATION),
        p(param::M),
        p(param::EF_CONSTRUCTION),
    );
    if q.is_some() || mm.is_some() || ef.is_some() {
        out.push_str(&format!(
            ", {}, {}, {}",
            lit(q.unwrap_or("bf16")),
            mm.unwrap_or("0"),
            ef.unwrap_or("0")
        ));
    }
    out.push(')');
    Ok(out)
}

/// `v` in the form of ClickHouse 24.8–25.7, which has no dimension:
/// `vector_similarity('hnsw', 'L2Distance')`, or the five arguments when a
/// quantization, `m` or `ef_construction` is set (0 for the ones that
/// aren't: the server's default).
pub fn legacy_index_type(v: &VectorIndex) -> String {
    let p = |k: &str| v.params.get(k).map(|x| x.trim()).filter(|x| !x.is_empty());
    let lit = |s: &str| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"));
    let algorithm = v
        .algorithm
        .as_deref()
        .filter(|a| !a.is_empty())
        .unwrap_or("hnsw");
    let m = v
        .metric
        .as_deref()
        .filter(|m| !m.is_empty())
        .unwrap_or(metric::L2);
    let native = METRICS
        .iter()
        .find(|(_, std)| *std == m)
        .map_or(m, |(n, _)| n);
    let mut out = format!("{KIND}({}, {}", lit(algorithm), lit(native));
    let (q, mm, ef) = (
        p(param::QUANTIZATION),
        p(param::M),
        p(param::EF_CONSTRUCTION),
    );
    if q.is_some() || mm.is_some() || ef.is_some() {
        out.push_str(&format!(
            ", {}, {}, {}",
            lit(q.unwrap_or("bf16")),
            mm.unwrap_or("0"),
            ef.unwrap_or("0")
        ));
    }
    out.push(')');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn which_arrays_are_vectors() {
        assert_eq!(
            meta("Array(Float32)"),
            Some(VectorMeta::dense(None, VectorElement::F32))
        );
        assert_eq!(
            meta("array(bfloat16)").unwrap().element,
            VectorElement::Bf16
        );
        for no in [
            "Array(Float64)",
            "Array(Int8)",
            "Float32",
            "Array(Array(Float32))",
        ] {
            assert_eq!(meta(no), None, "{no}");
        }
        assert_eq!(
            cell_text("[0.1,2,1e-7]").as_deref(),
            Some("[0.1,2,0.0000001]")
        );
        assert_eq!(cell_text("[nan,1]"), None);
        assert_eq!(literal(&json!([1.0, 2.5])).as_deref(), Some("[1,2.5]"));
        assert_eq!(literal(&json!("[1]")), None);
    }

    #[test]
    fn old_servers_say_what_they_lack() {
        // 22.8, 26.9 and 22.8, as they answer.
        let ix = "CREATE TABLE t (v Array(Float32), INDEX i v TYPE vector_similarity('hnsw', 'cosineDistance', 3) GRANULARITY 100000000) ENGINE = MergeTree ORDER BY tuple()";
        let hint = version_hint(
            ix,
            80,
            "Code: 80. DB::Exception: Unknown Index type 'vector_similarity'. (INCORRECT_QUERY)",
        )
        .unwrap();
        assert!(hint.contains("25.8") && hint.contains("26.4"), "{hint}");
        assert!(version_hint(
            ix,
            117,
            "Second argument (distance function) of vector similarity index is not supported"
        )
        .is_some());
        assert!(version_hint(
            "CREATE TABLE t (v Array(BFloat16)) ENGINE = Memory",
            50,
            "Code: 50. DB::Exception: Unknown data type family: BFloat16. (UNKNOWN_TYPE)"
        )
        .unwrap()
        .contains("25.1"));
        // Anything else says nothing more.
        assert!(version_hint(ix, 60, "Unknown table").is_none());
        assert!(version_hint("SELECT 1", 80, "x").is_none());
    }

    #[test]
    fn index_types_round_trip() {
        let v = parse_index("vector_similarity('hnsw', 'cosineDistance', 3)").unwrap();
        assert_eq!(v.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(v.metric.as_deref(), Some("cosine"));
        assert_eq!(v.params.get(DIMENSIONS).map(String::as_str), Some("3"));
        assert_eq!(
            index_type(&v).unwrap(),
            "vector_similarity('hnsw', 'cosineDistance', 3)"
        );
        let six = "vector_similarity('hnsw', 'L2Distance', 768, 'bf16', 32, 128)";
        let v = parse_index(six).unwrap();
        assert_eq!(
            v.params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>(),
            [
                "dimensions=768",
                "ef_construction=128",
                "m=32",
                "quantization=bf16"
            ]
        );
        assert_eq!(index_type(&v).unwrap(), six);
        // 0 is ClickHouse's default: left out, and written back as 0.
        let v = parse_index("vector_similarity('hnsw', 'dotProduct', 3, 'i8', 0, 0)").unwrap();
        assert_eq!(v.metric.as_deref(), Some("ip"));
        assert_eq!(v.params.len(), 2);
        assert_eq!(
            index_type(&v).unwrap(),
            "vector_similarity('hnsw', 'dotProduct', 3, 'i8', 0, 0)"
        );
        assert!(parse_index("bloom_filter(0.01)").is_none());
        // 24.8–25.7: no dimension; the quantization third.
        let v = parse_index("vector_similarity('hnsw', 'L2Distance')").unwrap();
        assert_eq!((v.metric.as_deref(), v.params.len()), (Some("l2"), 0));
        let v = parse_index("vector_similarity('hnsw', 'cosineDistance', 'f32', 16, 64)").unwrap();
        assert_eq!(
            v.params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>(),
            ["ef_construction=64", "m=16", "quantization=f32"]
        );
        let v =
            parse_index("vector_similarity('hnsw', 'L2Distance', 'bf16', 32, 128, 256)").unwrap();
        assert_eq!(v.params["ef_search"], "256");
        assert!(!v.params.contains_key(DIMENSIONS));
        assert!(index_type(&VectorIndex::default())
            .unwrap_err()
            .contains("dimensions"));
    }

    #[test]
    fn legacy_index_types() {
        let v = |metric: &str, params: &[(&str, &str)]| VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some(metric.into()),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        for (ix, want) in [
            (
                v("cosine", &[]),
                "vector_similarity('hnsw', 'cosineDistance')",
            ),
            (
                v("l2", &[("m", "16")]),
                "vector_similarity('hnsw', 'L2Distance', 'bf16', 16, 0)",
            ),
            (
                v("l2", &[("quantization", "f32"), ("ef_construction", "64")]),
                "vector_similarity('hnsw', 'L2Distance', 'f32', 0, 64)",
            ),
        ] {
            assert_eq!(legacy_index_type(&ix), want);
            // It reads back as the same settings (the defaults aside).
            let back = parse_index(want).unwrap();
            assert_eq!(back.metric, ix.metric);
        }
    }

    #[test]
    fn browse_targets() {
        let t = |db: Option<&str>, n: &str| Some((db.map(str::to_string), n.to_string()));
        assert_eq!(
            browse_target("SELECT *\nFROM `db`.`docs`\nLIMIT 1000"),
            t(Some("db"), "docs")
        );
        assert_eq!(
            browse_target("select * from docs where id > 1 order by id"),
            t(None, "docs")
        );
        assert_eq!(
            browse_target("SELECT *\nFROM `a\\`b`.`c d`\nWHERE x = 1;"),
            t(Some("a`b"), "c d")
        );
        assert_eq!(browse_target("SELECT * FROM `t`"), t(None, "t"));
        for no in [
            "SELECT id FROM t",
            "SELECT * FROM t JOIN u USING id",
            "SELECT * FROM t AS x",
            "SELECT * FROM t, u",
            "SELECT * FROM t,u",
            "SELECT * FROMt",
            "SELECT * FROM `t",
            "SELECT * FROM (SELECT 1)",
            "SELECT * FROM numbers(3)",
            "INSERT INTO t SELECT * FROM u",
        ] {
            assert_eq!(browse_target(no), None, "{no}");
        }
    }

    /// A browse marks the Float64 arrays a vector index searches, with the
    /// index's dimension, and writes their cells as vector text; other
    /// Float64 arrays stay general arrays.
    #[test]
    fn indexed_float64_arrays_are_vectors_in_a_browse() {
        let indexed = indexed_arrays(&[
            (
                "wide".into(),
                "vector_similarity('hnsw', 'L2Distance', 2)".into(),
            ),
            (
                "`old one`".into(),
                "vector_similarity('hnsw', 'L2Distance')".into(),
            ),
        ]);
        assert_eq!(
            indexed,
            [("wide".to_string(), Some(2)), ("old one".to_string(), None)]
        );
        let mut r = dbine_driver::StatementResult {
            columns: vec![
                dbine_driver::ResultColumn::new("id", "UInt64"),
                dbine_driver::ResultColumn::new("wide", "Array(Float64)"),
                dbine_driver::ResultColumn::new("old one", "Array(Float64)"),
                dbine_driver::ResultColumn::new("nums", "Array(Float64)"),
            ],
            rows: vec![vec![
                serde_json::json!(1),
                serde_json::json!("[0.5,1e-7]"),
                serde_json::json!("[1,2]"),
                serde_json::json!("[1e-7]"),
            ]],
            ..Default::default()
        };
        mark_indexed_arrays(&mut r, &indexed);
        assert_eq!(
            r.columns[1].vector,
            Some(VectorMeta::dense(Some(2), VectorElement::F64))
        );
        assert_eq!(
            r.columns[2].vector,
            Some(VectorMeta::dense(None, VectorElement::F64))
        );
        assert!(r.columns[3].vector.is_none() && r.columns[0].vector.is_none());
        assert_eq!(r.columns[1].type_name, "Array(Float64)");
        assert_eq!(
            r.rows[0],
            [
                serde_json::json!(1),
                serde_json::json!("[0.5,0.0000001]"),
                serde_json::json!("[1,2]"),
                serde_json::json!("[1e-7]")
            ]
        );
    }
}
