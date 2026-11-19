//! SQL Server's `vector(n[, float32|float16])` (SQL Server 2025, Azure SQL).
//!
//! The session asks for the vector TDS type (DBine's tiberius,
//! `Config::vector_support`), so a float32 vector column arrives as its own
//! type with its dimensions and its values in the binary form: an 8-byte
//! header (layout format 0xA9, layout version 1, element count as u16 LE,
//! element type, three reserved bytes), then the elements little-endian.
//! Version 1 of the feature covers float32 only: float16 vectors (a preview
//! feature) still arrive as JSON text, without the column saying it holds
//! vectors.
//!
//! The catalog (`sys.columns.vector_dimensions` / `vector_base_type`) gives
//! the declared type in describes, DDL, compare and clone; the catalog
//! queries carry them in place of a vector column's precision and scale.

use dbine_driver::vector::{self, VectorElement, VectorMeta};
use tiberius::{VectorBaseType, VectorType, VECTOR_HEADER_SIZE};

/// `IndexDef::kind` of a vector index (`sys.indexes.type` 8).
pub(crate) const INDEX_KIND: &str = "VECTOR";

const LAYOUT_FORMAT: u8 = 0xA9;
const LAYOUT_VERSION: u8 = 0x01;

/// The type as T-SQL declares it: `vector(3)`, `vector(3, float16)`; a
/// bare `vector` when the dimensions aren't known.
pub(crate) fn type_name(dims: Option<u32>, base: VectorBaseType) -> String {
    match (dims, base) {
        (Some(n), VectorBaseType::Float32) => format!("vector({n})"),
        (Some(n), b) => match b.name() {
            Some(t) => format!("vector({n}, {t})"),
            None => format!("vector({n})"),
        },
        (None, _) => "vector".into(),
    }
}

fn element(base: VectorBaseType) -> VectorElement {
    match base {
        VectorBaseType::Float32 => VectorElement::F32,
        VectorBaseType::Float16 => VectorElement::F16,
        VectorBaseType::Other(_) => VectorElement::Other,
    }
}

/// What a result column of this vector type says.
pub(crate) fn meta(t: &VectorType) -> VectorMeta {
    VectorMeta::dense(t.dimensions().map(u32::from), element(t.base_type))
}

/// The result column's type name for a vector type.
pub(crate) fn wire_type_name(t: &VectorType) -> String {
    type_name(t.dimensions().map(u32::from), t.base_type)
}

/// A vector's binary form as its JSON-array text; `None` when the bytes
/// aren't one.
pub(crate) fn decode(b: &[u8]) -> Option<String> {
    let (head, body) = b.split_at_checked(VECTOR_HEADER_SIZE)?;
    if head[0] != LAYOUT_FORMAT || head[1] != LAYOUT_VERSION {
        return None;
    }
    let count = u16::from_le_bytes([head[2], head[3]]) as usize;
    let base = VectorBaseType::from_code(head[4]);
    if base.size()? * count != body.len() {
        return None;
    }
    Some(match base {
        VectorBaseType::Float32 => vector::text(vector::from_f32_le(body)?),
        VectorBaseType::Float16 => vector::text(vector::from_f16_le(body)?),
        VectorBaseType::Other(_) => return None,
    })
}

/// A vector value as its binary form for a column of `base` elements.
pub(crate) fn encode(values: &[f64], base: VectorBaseType) -> Option<Vec<u8>> {
    let body = match base {
        VectorBaseType::Float32 => vector::to_f32_le(values),
        VectorBaseType::Float16 => vector::to_f16_le(values),
        VectorBaseType::Other(_) => return None,
    };
    let count = u16::try_from(values.len()).ok()?;
    let mut out = Vec::with_capacity(VECTOR_HEADER_SIZE + body.len());
    out.extend_from_slice(&[LAYOUT_FORMAT, LAYOUT_VERSION]);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&[base.code(), 0, 0, 0]);
    out.extend_from_slice(&body);
    Some(out)
}

/// The server's JSON text of a vector (`[1.0000000e+000,…]`, what a
/// float16 vector or a `CAST(v AS nvarchar(max))` gives) in the canonical
/// form, each number the shortest float32 that reads back the same.
pub(crate) fn canonical(text: &str) -> Option<String> {
    let v = vector::parse(text)?;
    Some(vector::text(v.into_iter().map(|x| x as f32)))
}

/// `format_type` for a `vector` column: `precision` carries its dimensions
/// and `scale` its element type when the catalog query read them (see
/// [`crate::SqlServerSession::vector_shape`]); otherwise the dimensions
/// come from the size as float32.
pub(crate) fn format_type(max_len: i32, precision: i32, scale: i32) -> String {
    let base = VectorBaseType::from_code(scale.clamp(0, 255) as u8);
    let dims = if precision > 0 {
        Some(precision as u32)
    } else {
        VectorType {
            size: max_len.clamp(0, u16::MAX as i32) as u16,
            base_type: VectorBaseType::Float32,
        }
        .dimensions()
        .map(u32::from)
    };
    type_name(dims, base)
}

/// What a catalog query's `CAST(c.precision AS int), CAST(c.scale AS int)`
/// becomes on a server whose `sys.columns` describes vector columns: their
/// dimensions and element type code in those two places.
pub(crate) const SHAPE: &str = "CAST(COALESCE(c.vector_dimensions, c.precision) AS int), \
     CAST(CASE WHEN c.vector_dimensions IS NULL THEN c.scale ELSE c.vector_base_type END AS int)";

/// The fragment [`SHAPE`] replaces.
pub(crate) const PRECISION_SCALE: &str = "CAST(c.precision AS int), CAST(c.scale AS int)";

/// Whether this server's `sys.columns` describes vector columns.
pub(crate) const CATALOG_SQL: &str =
    "SELECT CAST(CASE WHEN COL_LENGTH('sys.columns', 'vector_dimensions') IS NULL THEN 0 ELSE 1 END AS bit)";

/// A server's error about the vector type, a vector index or a `VECTOR_*`
/// function it doesn't know (SQL Server 2022 and before: 2715 "Cannot find
/// data type vector", 343 "Unknown object type 'VECTOR'", 195 "'VECTOR_…'
/// is not a recognized built-in function name"), with the releases that
/// have them: the designer offers them on every server.
pub(crate) fn with_release_hint(number: u32, message: &str) -> String {
    if matches!(number, 195 | 343 | 2715) && message.to_ascii_lowercase().contains("vector") {
        format!(
            "{message} — El tipo vector, los índices vectoriales y las funciones VECTOR_* son de SQL Server 2025 y Azure SQL."
        )
    } else {
        message.to_string()
    }
}

/// The table a browse query (`SELECT TOP (n) *\nFROM [s].[t]`, then maybe
/// its grid filter and sort) reads, as the bracketed name `OBJECT_ID`
/// takes; `None` for any other query.
pub(crate) fn browsed_table(sql: &str) -> Option<String> {
    let rest = sql.strip_prefix("SELECT TOP (")?;
    let close = rest.find(')')?;
    if close == 0 || !rest[..close].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut rest = rest[close..].strip_prefix(") *\nFROM ")?;
    let start = rest;
    let mut parts = 0;
    loop {
        // One `[name]`, `]]` inside it a `]`.
        let mut chars = rest.char_indices();
        if chars.next()?.1 != '[' {
            return None;
        }
        let mut end = None;
        let mut prev_close = false;
        for (i, c) in chars {
            if c == ']' {
                prev_close = !prev_close;
            } else if prev_close {
                end = Some(i);
                break;
            }
        }
        let end = match end {
            Some(e) => e,
            None if prev_close => rest.len(),
            None => return None,
        };
        parts += 1;
        rest = &rest[end..];
        match rest.strip_prefix('.') {
            Some(r) if parts < 2 => rest = r,
            Some(_) => return None,
            None => break,
        }
    }
    let name = &start[..start.len() - rest.len()];
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| name.to_string())
}

/// A table's float16 vector columns and their dimensions (`@P1`: the
/// table's name for `OBJECT_ID`).
pub(crate) const FLOAT16_COLUMNS_SQL: &str =
    "SELECT c.name, CAST(c.vector_dimensions AS int) FROM sys.columns c
  WHERE c.object_id = OBJECT_ID(@P1) AND c.vector_base_type = 1";

/// SQL Server's distance names (`sys.vector_indexes.distance_metric`,
/// `METRIC = '…'`) and the standard ones.
pub(crate) fn metric_of(native: &str) -> String {
    use dbine_driver::vector::metric;
    match native.trim().to_ascii_lowercase().as_str() {
        "cosine" => metric::COSINE.into(),
        "euclidean" => metric::L2.into(),
        "dot" => metric::IP.into(),
        other => other.into(),
    }
}

/// A vector index's settings from `sys.vector_indexes`' type and distance
/// (`DiskANN`, `COSINE`): the algorithm DiskANN when it wasn't read, the
/// standard distance names. DiskANN's build parameters (R, L, M, StartId)
/// are the server's own: `CREATE VECTOR INDEX` doesn't take them, so they
/// aren't the index's settings.
pub(crate) fn index(kind: Option<&str>, metric: Option<&str>) -> dbine_driver::VectorIndex {
    dbine_driver::VectorIndex {
        algorithm: Some(kind.unwrap_or("DiskANN").trim().to_ascii_lowercase()),
        metric: metric.map(metric_of),
        params: Default::default(),
    }
}

/// The `METRIC = '…'` of a standard (or native) distance name.
pub(crate) fn native_metric(standard: &str) -> Option<&'static str> {
    use dbine_driver::vector::metric;
    match standard.trim().to_ascii_lowercase().as_str() {
        metric::COSINE => Some("cosine"),
        metric::L2 | "euclidean" => Some("euclidean"),
        metric::IP | "dot" => Some("dot"),
        _ => None,
    }
}

/// The value of a vector edit as a T-SQL literal: its JSON text in a
/// Unicode string, which the server converts on insert or update.
pub(crate) fn literal(v: &serde_json::Value) -> Option<String> {
    vector::value_text(v).map(|t| format!("N'{t}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_form_round_trips() {
        // What the server sent for [1,2,3] and [0.1,0.2,0.3,0.4].
        let a = [
            169u8, 1, 3, 0, 0, 0, 0, 0, 0, 0, 128, 63, 0, 0, 0, 64, 0, 0, 64, 64,
        ];
        assert_eq!(decode(&a).as_deref(), Some("[1,2,3]"));
        let b = [
            169u8, 1, 4, 0, 0, 0, 0, 0, 205, 204, 204, 61, 205, 204, 76, 62, 154, 153, 153, 62,
            205, 204, 204, 62,
        ];
        assert_eq!(decode(&b).as_deref(), Some("[0.1,0.2,0.3,0.4]"));
        assert_eq!(
            encode(&[1.0, 2.0, 3.0], VectorBaseType::Float32).unwrap(),
            a
        );
        let h = encode(&[1.5, -2.0], VectorBaseType::Float16).unwrap();
        assert_eq!(h.len(), 8 + 4);
        assert_eq!(h[4], 1);
        assert_eq!(decode(&h).as_deref(), Some("[1.5,-2]"));
        // Not a vector: wrong magic, short, a count that doesn't match.
        assert_eq!(decode(&[0u8; 12]), None);
        assert_eq!(decode(&a[..5]), None);
        let mut bad = a;
        bad[2] = 4;
        assert_eq!(decode(&bad), None);
    }

    #[test]
    fn older_servers_say_which_release_has_vectors() {
        for (n, m) in [
            (
                2715,
                "Column, parameter, or variable #2: Cannot find data type vector.",
            ),
            (
                343,
                "Unknown object type 'VECTOR' used in a CREATE, DROP, or ALTER statement.",
            ),
            (
                195,
                "'VECTOR_DISTANCE' is not a recognized built-in function name.",
            ),
        ] {
            assert!(
                with_release_hint(n, m).ends_with("son de SQL Server 2025 y Azure SQL."),
                "{n}"
            );
        }
        assert_eq!(
            with_release_hint(2715, "Cannot find data type foo."),
            "Cannot find data type foo."
        );
        assert_eq!(
            with_release_hint(208, "Invalid object name 'vector'."),
            "Invalid object name 'vector'."
        );
    }

    #[test]
    fn browse_queries_name_their_table() {
        use dbine_driver::sql::{select_top, Limit, Quote};
        let browse = select_top(Quote::Bracket, Limit::Top, Some("dbo"), "docs", 200);
        assert_eq!(browsed_table(&browse).as_deref(), Some("[dbo].[docs]"));
        // Filtered and sorted by the grid.
        assert_eq!(
            browsed_table(&format!("{browse}\nWHERE [id] > 1\nORDER BY [id] DESC")).as_deref(),
            Some("[dbo].[docs]")
        );
        let odd = select_top(Quote::Bracket, Limit::Top, Some("s]x"), "a b.c", 5);
        assert_eq!(browsed_table(&odd).as_deref(), Some("[s]]x].[a b.c]"));
        let bare = select_top(Quote::Bracket, Limit::Top, None, "t", 5);
        assert_eq!(browsed_table(&bare).as_deref(), Some("[t]"));
        for other in [
            "SELECT * FROM [dbo].[docs]",
            "SELECT TOP (5) id FROM [dbo].[docs]",
            "SELECT TOP (x) *\nFROM [dbo].[docs]",
            "SELECT TOP (5) *\nFROM dbo.docs",
            "SELECT TOP (5) *\nFROM [a].[b].[c]",
            "SELECT TOP (5) *\nFROM [dbo].[docs]JOIN",
            "SELECT TOP (5) *\nFROM [dbo",
        ] {
            assert_eq!(browsed_table(other), None, "{other}");
        }
    }

    #[test]
    fn server_text_becomes_canonical() {
        assert_eq!(
            canonical("[1.0000000e+000,2.0000000e+000,3.0000001e-001]").as_deref(),
            Some("[1,2,0.3]")
        );
        assert_eq!(canonical("[]").as_deref(), Some("[]"));
        assert_eq!(canonical("nope"), None);
    }

    #[test]
    fn declared_types() {
        assert_eq!(type_name(Some(3), VectorBaseType::Float32), "vector(3)");
        assert_eq!(
            type_name(Some(3), VectorBaseType::Float16),
            "vector(3, float16)"
        );
        // From the catalog: dims in precision, element type in scale; or
        // from the size alone.
        assert_eq!(format_type(14, 3, 1), "vector(3, float16)");
        assert_eq!(format_type(20, 3, 0), "vector(3)");
        assert_eq!(format_type(20, 0, 0), "vector(3)");
        assert_eq!(format_type(0, 0, 0), "vector");
    }

    #[test]
    fn metrics_and_literals() {
        assert_eq!(metric_of("COSINE"), "cosine");
        assert_eq!(metric_of("EUCLIDEAN"), "l2");
        assert_eq!(metric_of("DOT"), "ip");
        assert_eq!(native_metric("l2"), Some("euclidean"));
        assert_eq!(native_metric("ip"), Some("dot"));
        assert_eq!(native_metric("hamming"), None);
        assert_eq!(
            literal(&serde_json::json!([1.0, 2.5, -3])).as_deref(),
            Some("N'[1,2.5,-3]'")
        );
        assert_eq!(literal(&serde_json::json!(["a"])), None);
    }

    #[test]
    fn result_metadata() {
        let t = VectorType {
            size: 20,
            base_type: VectorBaseType::Float32,
        };
        assert_eq!(wire_type_name(&t), "vector(3)");
        assert_eq!(meta(&t), VectorMeta::dense(Some(3), VectorElement::F32));
    }
}
