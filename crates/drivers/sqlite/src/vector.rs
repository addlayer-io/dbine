//! libSQL's vector columns (`F32_BLOB(3)`, `FLOAT64(768)`…): which declared
//! types they are, and their blobs as DBine's vector text and back.
//!
//! libSQL keeps a vector in a blob. `F32` is its little-endian floats, with
//! nothing else (an even length); the other formats end with a type byte
//! (an odd length): 2 `F64`, 3 `1BIT`, 4 `8`, 5 `F16`, 6 `BF16`, so a blob
//! says what it holds. Checked against sqld 0.24 (`hex(vector64('[1,2,3]'))`
//! and friends):
//!
//! - `F64`, `F16`, `BF16`: the little-endian numbers, then the type byte.
//! - `8`: one byte per dimension `q`, zeros up to a multiple of 4, `alpha`
//!   and `shift` (little-endian `f32`), a zero byte, the number of padding
//!   bytes, the type byte; a value is `alpha · q + shift`.
//! - `1BIT`: the bits, the first dimension in the lowest bit; a zero byte
//!   when needed to keep the length odd; `8 · (length − 1) − dims`; the
//!   type byte. A bit reads as 1 or −1, as `vector_extract` writes it.
//!
//! A SQLite file written by libSQL keeps these columns, so the SQLite driver
//! reads them too (it can't create their indexes: `libsql_vector_idx` is
//! libSQL's).

use dbine_driver::transfer::Cell;
use dbine_driver::vector as v;
use dbine_driver::{VectorElement, VectorMeta};

/// A libSQL vector format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    F32,
    F64,
    F16,
    Bf16,
    /// 8-bit, scaled (`alpha · q + shift`).
    F8,
    /// One bit per dimension.
    Bit,
}

impl Kind {
    /// The column type (`F32_BLOB`…).
    pub fn type_name(self) -> &'static str {
        match self {
            Kind::F32 => "F32_BLOB",
            Kind::F64 => "F64_BLOB",
            Kind::F16 => "F16_BLOB",
            Kind::Bf16 => "FB16_BLOB",
            Kind::F8 => "F8_BLOB",
            Kind::Bit => "F1BIT_BLOB",
        }
    }

    /// The function that makes one from text (`vector32('[1,2]')`).
    pub fn function(self) -> &'static str {
        match self {
            Kind::F32 => "vector32",
            Kind::F64 => "vector64",
            Kind::F16 => "vector16",
            Kind::Bf16 => "vectorb16",
            Kind::F8 => "vector8",
            Kind::Bit => "vector1bit",
        }
    }

    /// Its numbers as DBine names them. The 8-bit format is libSQL's own
    /// (scaled bytes that read as floats): neither `i8` nor `u8`.
    pub fn element(self) -> VectorElement {
        match self {
            Kind::F32 => VectorElement::F32,
            Kind::F64 => VectorElement::F64,
            Kind::F16 => VectorElement::F16,
            Kind::Bf16 => VectorElement::Bf16,
            Kind::F8 => VectorElement::Other,
            Kind::Bit => VectorElement::Bit,
        }
    }

    /// The format for a vector of `e`s: the one that holds them exactly
    /// (bytes fit in `F32`).
    pub fn for_element(e: VectorElement) -> Kind {
        match e {
            VectorElement::F64 => Kind::F64,
            VectorElement::F16 => Kind::F16,
            VectorElement::Bf16 => Kind::Bf16,
            VectorElement::Bit => Kind::Bit,
            VectorElement::F32 | VectorElement::I8 | VectorElement::U8 | VectorElement::Other => {
                Kind::F32
            }
        }
    }
}

/// The vector format a declared column type names, and its dimensions:
/// `F32_BLOB(3)`, `F64_BLOB`, `FLOAT32(3)`… The `FLOATnn` spellings count
/// only with a dimension: a bare `FLOAT64` is an ordinary REAL column.
pub fn column_type(decl: &str) -> Option<(Kind, Option<u32>)> {
    let decl = decl.trim();
    let (name, dims) = match decl.find('(') {
        Some(p) => {
            let args = decl[p + 1..].trim_end().strip_suffix(')')?;
            (decl[..p].trim(), Some(args.trim().parse::<u32>().ok()?))
        }
        None => (decl, None),
    };
    let name = name.to_ascii_uppercase();
    let (kind, blob) = match name.as_str() {
        "F32_BLOB" => (Kind::F32, true),
        "F64_BLOB" => (Kind::F64, true),
        "F16_BLOB" => (Kind::F16, true),
        "FB16_BLOB" => (Kind::Bf16, true),
        "F8_BLOB" => (Kind::F8, true),
        "F1BIT_BLOB" => (Kind::Bit, true),
        "FLOAT32" => (Kind::F32, false),
        "FLOAT64" => (Kind::F64, false),
        "FLOAT16" => (Kind::F16, false),
        "FLOATB16" => (Kind::Bf16, false),
        "FLOAT8" => (Kind::F8, false),
        "FLOAT1BIT" => (Kind::Bit, false),
        _ => return None,
    };
    (blob || dims.is_some()).then_some((kind, dims))
}

/// What a result column of this declared type holds, when it's a vector.
pub fn meta(decl: &str) -> Option<VectorMeta> {
    column_type(decl).map(|(k, dims)| VectorMeta::dense(dims, k.element()))
}

/// A libSQL vector blob as DBine's vector text (`[1,2.5]`); `None` when
/// the bytes aren't one.
pub fn text(b: &[u8]) -> Option<String> {
    if b.len().is_multiple_of(2) {
        return v::from_f32_le(b).map(v::text);
    }
    let (&ty, body) = b.split_last()?;
    match ty {
        2 => v::from_f64_le(body).map(v::text),
        5 => v::from_f16_le(body).map(v::text),
        6 => v::from_bf16_le(body).map(v::text),
        4 => {
            // q…, padding, alpha, shift, 0, padding count.
            let (&pad, rest) = body.split_last()?;
            let rest = rest.split_last()?.1;
            let dims = rest.len().checked_sub(8 + pad as usize)?;
            let f = |at: usize| Some(f32::from_le_bytes(rest.get(at..at + 4)?.try_into().ok()?));
            let (alpha, shift) = (f(dims + pad as usize)?, f(dims + pad as usize + 4)?);
            Some(v::text(
                rest[..dims].iter().map(|&q| alpha * q as f32 + shift),
            ))
        }
        3 => {
            let (&left, rest) = body.split_last()?;
            let dims = (8 * body.len()).checked_sub(left as usize)?;
            if dims > 8 * rest.len() {
                return None;
            }
            Some(v::text((0..dims).map(|i| {
                if rest[i / 8] >> (i % 8) & 1 == 1 {
                    1i8
                } else {
                    -1
                }
            })))
        }
        _ => None,
    }
}

/// A vector's numbers in libSQL's `kind` blob (what [`text`] reads).
pub fn encode(kind: Kind, values: &[f64]) -> Vec<u8> {
    match kind {
        Kind::F32 => v::to_f32_le(values),
        Kind::F64 => [v::to_f64_le(values), vec![2]].concat(),
        Kind::F16 => [v::to_f16_le(values), vec![5]].concat(),
        Kind::Bf16 => [v::to_bf16_le(values), vec![6]].concat(),
        Kind::F8 => {
            // As libSQL quantizes: `alpha = (max − min) / 255`, `shift =
            // min`, each value rounded to its nearest step, in f32.
            let f: Vec<f32> = values.iter().map(|&x| x as f32).collect();
            let min = f.iter().copied().fold(f32::INFINITY, f32::min);
            let max = f.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let (alpha, shift) = if f.is_empty() {
                (0.0, 0.0)
            } else {
                ((max - min) / 255.0, min)
            };
            let pad = (4 - f.len() % 4) % 4;
            let mut out: Vec<u8> = f
                .iter()
                .map(|&x| {
                    if alpha > 0.0 {
                        ((x - shift) / alpha).round().clamp(0.0, 255.0) as u8
                    } else {
                        0
                    }
                })
                .collect();
            out.extend(std::iter::repeat_n(0, pad));
            out.extend(alpha.to_le_bytes());
            out.extend(shift.to_le_bytes());
            out.extend([0, pad as u8, 4]);
            out
        }
        Kind::Bit => {
            let mut out = vec![0u8; values.len().div_ceil(8)];
            for (i, &x) in values.iter().enumerate() {
                if x > 0.0 {
                    out[i / 8] |= 1 << (i % 8);
                }
            }
            if out.len().is_multiple_of(2) {
                out.push(0);
            }
            let left = 8 * (out.len() + 1) - values.len();
            out.extend([left as u8, 3]);
            out
        }
    }
}

/// A result cell of a vector column: a blob as vector text, text that
/// holds an array as the canonical one; anything else as it is (`None`).
pub fn cell_text(blob: Option<&[u8]>, text_value: Option<&str>) -> Option<String> {
    match (blob, text_value) {
        (Some(b), _) => text(b),
        (None, Some(t)) => v::parse(t).map(v::text),
        _ => None,
    }
}

/// A transfer cell read from a vector column: its blob as DBine's vector
/// text (`Cell::Text`, what every target loads), text holding an array as
/// the canonical one; anything else as it is.
pub fn read_cell(c: Cell) -> Cell {
    match c {
        Cell::Bytes(b) => match text(&b) {
            Some(t) => Cell::Text(t),
            None => Cell::Bytes(b),
        },
        Cell::Text(t) => match v::parse(&t) {
            Some(n) => Cell::Text(v::text(n)),
            None => Cell::Text(t),
        },
        other => other,
    }
}

/// A transfer cell going into a vector column of format `kind`: vector
/// text (or JSON) as libSQL's blob. `None`: it goes as it is (a blob
/// already, a NULL…).
pub fn load_cell(kind: Kind, c: &Cell) -> Option<Cell> {
    match c {
        Cell::Text(s) | Cell::Json(s) => v::parse(s).map(|n| Cell::Bytes(encode(kind, &n))),
        _ => None,
    }
}

/// The numbers of a vector value of an edit or insert script: a JSON array
/// of numbers (text that looks like one stays text).
fn script_vector(value: &serde_json::Value) -> Option<Vec<f64>> {
    match value {
        serde_json::Value::Array(_) => v::parse_value(value),
        _ => None,
    }
}

/// Where a script runs: libSQL makes vectors with its functions
/// (`vector64('[…]')`); SQLite, which has none, writes each format's blob
/// (`X'…'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Libsql,
    Sqlite,
}

/// Each format with the declared types that name it, as `LIKE` patterns
/// (the `FLOATnn` spellings only with a dimension: a bare `FLOAT8` is a
/// REAL).
const FORMATS: [(Kind, &str, &str); 6] = [
    (Kind::F32, "F32%", "FLOAT32(%"),
    (Kind::F64, "F64%", "FLOAT64(%"),
    (Kind::F16, "F16%", "FLOAT16(%"),
    (Kind::Bf16, "FB16%", "FLOATB16(%"),
    (Kind::F8, "F8%", "FLOAT8(%"),
    (Kind::Bit, "F1BIT%", "FLOAT1BIT(%"),
];

fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The SQL that writes a vector value of an edit or insert script into
/// `column` of `table` when the script doesn't know the column's type (an
/// untyped script, or a type that isn't libSQL's): the statement reads it
/// (`pragma_table_info`) and makes the vector in that column's format: `F64_BLOB` gets `vector64('[…]')` (SQLite: the F64
/// blob), and so on; a column that isn't a vector column gets the array's
/// text; one the catalog doesn't show, the usual `F32`. `None`: not a
/// vector value.
pub fn literal(
    engine: Engine,
    table: &dbine_driver::ObjectRef,
    column: &str,
    value: &serde_json::Value,
) -> Option<String> {
    let values = script_vector(value)?;
    let text = v::text(values.iter().copied());
    let make = |k: Kind| match engine {
        Engine::Libsql => format!("{}(x.v)", k.function()),
        Engine::Sqlite => {
            let hex: String = encode(k, &values)
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect();
            format!("X'{hex}'")
        }
    };
    let mut case = String::from("CASE");
    for (k, blob, float) in FORMATS {
        case.push_str(&format!(
            " WHEN t.type LIKE '{blob}' OR t.type LIKE '{float}' THEN {}",
            make(k)
        ));
    }
    case.push_str(&format!(
        " WHEN t.type IS NULL THEN {} ELSE x.v END",
        make(Kind::F32)
    ));
    let info = match table.schema().filter(|s| !s.is_empty()) {
        Some(sch) => format!(
            "pragma_table_info({}, {})",
            quoted(&table.name),
            quoted(sch)
        ),
        None => format!("pragma_table_info({})", quoted(&table.name)),
    };
    Some(format!(
        "(SELECT {case} FROM (SELECT {} AS v) AS x LEFT JOIN {info} AS t ON t.name = {} COLLATE NOCASE)",
        quoted(&text),
        quoted(column)
    ))
}

/// The vector format of a column whose type a typed script gives
/// (`ResultColumn::type_name`: `F64_BLOB(3)`, `FLOAT16(2)`…); `None` when
/// that type isn't one of libSQL's vector types, or isn't given.
pub fn kind_of(c: &dbine_driver::ResultColumn) -> Option<Kind> {
    column_type(&c.type_name).map(|(k, _)| k)
}

/// The SQL that writes a vector value into a column of format `kind`, known
/// from the script's types: `vector64('[…]')` on libSQL, the format's blob
/// (`X'…'`) on SQLite. `None`: not a vector value.
pub fn typed_literal(engine: Engine, kind: Kind, value: &serde_json::Value) -> Option<String> {
    let values = script_vector(value)?;
    Some(match engine {
        Engine::Libsql => format!(
            "{}({})",
            kind.function(),
            quoted(&v::text(values.iter().copied()))
        ),
        Engine::Sqlite => {
            let hex: String = encode(kind, &values)
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect();
            format!("X'{hex}'")
        }
    })
}

/// Insert, update and delete scripts as the driver contract's defaults
/// write them for SQLite, with vectors as [`typed_literal`] writes them
/// where the script knows the column's format, and as [`literal`] (which
/// reads it from the catalog when the statement runs) where it doesn't.
pub mod scripts {
    use super::{kind_of, literal, typed_literal, Engine, Kind};
    use dbine_driver::ddl::{self, SqlFlavor};
    use dbine_driver::sql::{qualified_name, quote_ident, Quote};
    use dbine_driver::{ObjectRef, ResultColumn, RowChange};
    use serde_json::Value;

    const FLAVOR: SqlFlavor = SqlFlavor {
        quote: Quote::Double,
        ..SqlFlavor::ansi()
    };

    fn lit(
        engine: Engine,
        target: &ObjectRef,
        column: &str,
        kind: Option<Kind>,
        v: &Value,
    ) -> String {
        match kind {
            Some(k) => typed_literal(engine, k, v),
            None => literal(engine, target, column, v),
        }
        .unwrap_or_else(|| ddl::sql_literal(&FLAVOR, v))
    }

    /// `INSERT … VALUES` 100 rows a statement, as the contract's.
    pub fn insert(
        engine: Engine,
        target: &ObjectRef,
        columns: &[String],
        rows: &[Vec<Value>],
    ) -> String {
        insert_with(engine, target, columns, &vec![None; columns.len()], rows)
    }

    /// [`insert`] knowing the target columns' types, in the rows' order.
    pub fn insert_typed(
        engine: Engine,
        target: &ObjectRef,
        columns: &[ResultColumn],
        rows: &[Vec<Value>],
    ) -> String {
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let kinds: Vec<Option<Kind>> = columns.iter().map(kind_of).collect();
        insert_with(engine, target, &names, &kinds, rows)
    }

    fn insert_with(
        engine: Engine,
        target: &ObjectRef,
        columns: &[String],
        kinds: &[Option<Kind>],
        rows: &[Vec<Value>],
    ) -> String {
        let name = qualified_name(
            FLAVOR.quote,
            target.schema().filter(|s| !s.is_empty()),
            &target.name,
        );
        let cols: Vec<String> = columns
            .iter()
            .map(|c| quote_ident(FLAVOR.quote, c))
            .collect();
        let head = format!("INSERT INTO {name} ({}) VALUES", cols.join(", "));
        rows.chunks(100)
            .map(|chunk| {
                let tuples: Vec<String> = chunk
                    .iter()
                    .map(|r| {
                        let vals: Vec<String> = r
                            .iter()
                            .enumerate()
                            .map(|(i, v)| match columns.get(i) {
                                Some(c) => {
                                    lit(engine, target, c, kinds.get(i).copied().flatten(), v)
                                }
                                None => ddl::sql_literal(&FLAVOR, v),
                            })
                            .collect();
                        format!("({})", vals.join(", "))
                    })
                    .collect();
                format!("{head}\n  {};", tuples.join(",\n  "))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `UPDATE … SET … WHERE …;` per changed row, as the contract's (vector
    /// columns are never in the key).
    pub fn update(engine: Engine, target: &ObjectRef, changes: &[RowChange]) -> String {
        update_typed(engine, target, changes, &[])
    }

    /// [`update`] knowing the target's column types, by name (SQLite's
    /// names ignore case); a column not listed is untyped.
    pub fn update_typed(
        engine: Engine,
        target: &ObjectRef,
        changes: &[RowChange],
        types: &[ResultColumn],
    ) -> String {
        let kind = |c: &str| {
            types
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(c))
                .and_then(kind_of)
        };
        let name = qualified_name(
            FLAVOR.quote,
            target.schema().filter(|s| !s.is_empty()),
            &target.name,
        );
        let q = |c: &str| quote_ident(FLAVOR.quote, c);
        changes
            .iter()
            .filter(|c| !c.set.is_empty())
            .map(|c| {
                let set: Vec<String> = c
                    .set
                    .iter()
                    .map(|(k, v)| format!("{} = {}", q(k), lit(engine, target, k, kind(k), v)))
                    .collect();
                let wh: Vec<String> = c
                    .key
                    .iter()
                    .map(|(k, v)| {
                        if v.is_null() {
                            format!("{} IS NULL", q(k))
                        } else {
                            format!("{} = {}", q(k), ddl::sql_literal(&FLAVOR, v))
                        }
                    })
                    .collect();
                if wh.is_empty() {
                    format!("UPDATE {name} SET {};", set.join(", "))
                } else {
                    format!(
                        "UPDATE {name} SET {} WHERE {};",
                        set.join(", "),
                        wh.join(" AND ")
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn delete(target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> String {
        ddl::delete_script_with(FLAVOR.quote, target.schema(), &target.name, keys, &|v| {
            ddl::sql_literal(&FLAVOR, v)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn declared_types() {
        assert_eq!(column_type("F32_BLOB(3)"), Some((Kind::F32, Some(3))));
        assert_eq!(
            column_type("f64_blob ( 768 )"),
            Some((Kind::F64, Some(768)))
        );
        assert_eq!(column_type("FB16_BLOB"), Some((Kind::Bf16, None)));
        assert_eq!(column_type("FLOAT1BIT(8)"), Some((Kind::Bit, Some(8))));
        assert_eq!(column_type("FLOAT8(4)"), Some((Kind::F8, Some(4))));
        // A bare FLOAT64 is a REAL; other types aren't vectors.
        for no in ["FLOAT64", "FLOAT", "BLOB", "F32_BLOB(x)", "INTEGER", ""] {
            assert_eq!(column_type(no), None, "{no}");
        }
        assert_eq!(
            meta("F16_BLOB(2)"),
            Some(VectorMeta::dense(Some(2), VectorElement::F16))
        );
    }

    /// The blobs sqld 0.24 wrote for these vectors (`hex(vector…('[…]'))`),
    /// and what `vector_extract` showed for them.
    #[test]
    fn libsql_blobs_decode() {
        let cases = [
            ("0000803F0000004000004040", "[1,2,3]"),
            (
                "000000000000F03F0000000000000040000000000000084002",
                "[1,2,3]",
            ),
            ("003C0040004205", "[1,2,3]"),
            ("803F0040404006", "[1,2,3]"),
            ("050D03", "[1,-1,1]"),
            ("CD01001703", "[1,-1,1,1,-1,-1,1,1,1]"),
            ("FFFFFF01001703", &format!("[{}]", ["1"; 25].join(","))),
            // 8-bit: [0.5,-1,3,7] as libSQL quantized it.
            (
                "30007FFF8180003D000080BF000004",
                "[0.5058825,-1,2.984314,7]",
            ),
            // Its padding bytes are whatever was in memory.
            ("0000696E000000000000803F000204", "[1,1]"),
        ];
        for (hex, want) in cases {
            assert_eq!(text(&unhex(hex)).as_deref(), Some(want), "{hex}");
        }
        assert_eq!(text(&[1, 2, 3, 4, 5, 9]), None, "even, but not f32s");
        assert_eq!(text(&[1, 2, 7]), None, "unknown type byte");
        assert_eq!(text(&[]).as_deref(), Some("[]"));
    }

    #[test]
    fn encoding_matches_libsql() {
        let v = [1.0, 2.0, 3.0];
        assert_eq!(encode(Kind::F32, &v), unhex("0000803F0000004000004040"));
        assert_eq!(
            encode(Kind::F64, &v),
            unhex("000000000000F03F0000000000000040000000000000084002")
        );
        assert_eq!(encode(Kind::F16, &v), unhex("003C0040004205"));
        assert_eq!(encode(Kind::Bf16, &v), unhex("803F0040404006"));
        assert_eq!(encode(Kind::Bit, &[1.0, -2.0, 3.0]), unhex("050D03"));
        assert_eq!(
            encode(Kind::Bit, &[1.0, -1.0, 1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0]),
            unhex("CD01001703")
        );
        for d in [1, 2, 7, 8, 9, 15, 16, 17, 24, 25] {
            let ones = vec![1.0; d];
            let b = encode(Kind::Bit, &ones);
            assert_eq!(b.len() % 2, 1, "{d}");
            assert_eq!(text(&b), Some(v::text(ones.iter().map(|&x| x as f32))));
        }
        assert_eq!(
            encode(Kind::F8, &v),
            unhex("007FFF008180003C0000803F000104")
        );
        assert_eq!(
            encode(Kind::F8, &[0.5, -1.0, 3.0, 7.0]),
            unhex("30007FFF8180003D000080BF000004")
        );
        // Round trips through the decoder for every format.
        let w = [0.25, -1.5, 3.0, 0.0, 8.0];
        for k in [Kind::F32, Kind::F64, Kind::F16, Kind::Bf16] {
            assert_eq!(text(&encode(k, &w)).as_deref(), Some("[0.25,-1.5,3,0,8]"));
        }
        let q = text(&encode(Kind::F8, &w)).unwrap();
        let back = v::parse(&q).unwrap();
        assert!(back.iter().zip(w).all(|(a, b)| (a - b).abs() < 0.03), "{q}");
    }

    #[test]
    fn transfer_cells() {
        let blob = encode(Kind::F16, &[1.0, -2.0]);
        assert_eq!(read_cell(Cell::Bytes(blob)), Cell::Text("[1,-2]".into()));
        assert_eq!(
            read_cell(Cell::Text("[1.0]".into())),
            Cell::Text("[1]".into())
        );
        assert_eq!(read_cell(Cell::Null), Cell::Null);
        assert_eq!(
            load_cell(Kind::F64, &Cell::Text("[1.5]".into())),
            Some(Cell::Bytes(encode(Kind::F64, &[1.5])))
        );
        assert_eq!(load_cell(Kind::F32, &Cell::Bytes(vec![0; 4])), None);
        assert_eq!(load_cell(Kind::F32, &Cell::Text("no".into())), None);
    }

    #[test]
    fn script_literals() {
        let val = serde_json::json!([1, 2.5]);
        let t = dbine_driver::ObjectRef {
            kind: "table".into(),
            schema: None,
            name: "t'x".into(),
        };
        // libSQL: the column's own format, read from the catalog.
        let lib = literal(Engine::Libsql, &t, "v", &val).unwrap();
        assert_eq!(
            lib,
            "(SELECT CASE WHEN t.type LIKE 'F32%' OR t.type LIKE 'FLOAT32(%' THEN vector32(x.v) \
             WHEN t.type LIKE 'F64%' OR t.type LIKE 'FLOAT64(%' THEN vector64(x.v) \
             WHEN t.type LIKE 'F16%' OR t.type LIKE 'FLOAT16(%' THEN vector16(x.v) \
             WHEN t.type LIKE 'FB16%' OR t.type LIKE 'FLOATB16(%' THEN vectorb16(x.v) \
             WHEN t.type LIKE 'F8%' OR t.type LIKE 'FLOAT8(%' THEN vector8(x.v) \
             WHEN t.type LIKE 'F1BIT%' OR t.type LIKE 'FLOAT1BIT(%' THEN vector1bit(x.v) \
             WHEN t.type IS NULL THEN vector32(x.v) ELSE x.v END \
             FROM (SELECT '[1,2.5]' AS v) AS x LEFT JOIN pragma_table_info('t''x') AS t ON t.name = 'v' COLLATE NOCASE)"
        );
        // SQLite: each format's blob.
        let lite = literal(
            Engine::Sqlite,
            &dbine_driver::ObjectRef {
                schema: Some("main".into()),
                ..t.clone()
            },
            "v",
            &val,
        )
        .unwrap();
        assert!(
            lite.contains("LIKE 'F32%' OR t.type LIKE 'FLOAT32(%' THEN X'0000803F00002040'"),
            "{lite}"
        );
        assert!(
            lite.contains("THEN X'000000000000F03F000000000000044002'"),
            "{lite}"
        );
        assert!(
            lite.contains("WHEN t.type IS NULL THEN X'0000803F00002040' ELSE x.v END"),
            "{lite}"
        );
        assert!(lite.contains("pragma_table_info('t''x', 'main')"), "{lite}");
        assert_eq!(
            literal(Engine::Libsql, &t, "v", &serde_json::json!("x")),
            None
        );
        // Text that looks like a vector is still text.
        assert_eq!(
            literal(Engine::Libsql, &t, "v", &serde_json::json!("[1,2]")),
            None
        );
        let rows = vec![vec![serde_json::json!(1), val.clone(), "[3]".into()]];
        let cols = ["id".to_string(), "v".into(), "s".into()];
        let ins = scripts::insert(Engine::Libsql, &t, &cols, &rows);
        assert_eq!(
            ins,
            format!("INSERT INTO \"t'x\" (\"id\", \"v\", \"s\") VALUES\n  (1, {lib}, '[3]');")
        );
        let change = dbine_driver::RowChange {
            key: vec![
                ("id".into(), serde_json::json!(1)),
                ("k".into(), serde_json::Value::Null),
            ],
            set: vec![
                ("v".into(), serde_json::json!([0.5])),
                ("s".into(), "a".into()),
            ],
            row: Vec::new(),
        };
        let up = scripts::update(Engine::Sqlite, &t, &[change]);
        assert!(
            up.starts_with("UPDATE \"t'x\" SET \"v\" = (SELECT CASE WHEN t.type LIKE 'F32%' OR t.type LIKE 'FLOAT32(%' THEN X'0000003F'")
                && up.ends_with("AS t ON t.name = 'v' COLLATE NOCASE), \"s\" = 'a' WHERE \"id\" = 1 AND \"k\" IS NULL;"),
            "{up}"
        );
        assert_eq!(
            cell_text(None, Some(" [1, 2.50] ")).as_deref(),
            Some("[1,2.5]")
        );
        assert_eq!(cell_text(None, Some("hola")), None);
    }

    /// With the target's types, each vector goes straight in its column's
    /// format (no catalog lookup); a column whose type isn't libSQL's, or
    /// that isn't listed, still reads it from the catalog. The blobs are
    /// sqld 0.24's `hex(vector64('[1,2.5]'))` and friends (its 8-bit one
    /// pads with whatever was in memory: only the start is compared).
    #[test]
    fn typed_script_literals() {
        use dbine_driver::ResultColumn;
        let t = dbine_driver::ObjectRef {
            kind: "table".into(),
            schema: None,
            name: "docs".into(),
        };
        let val = serde_json::json!([1, 2.5]);
        for (ty, lib, lite) in [
            ("F32_BLOB(2)", "vector32('[1,2.5]')", "X'0000803F00002040'"),
            (
                "F64_BLOB(2)",
                "vector64('[1,2.5]')",
                "X'000000000000F03F000000000000044002'",
            ),
            ("f16_blob(2)", "vector16('[1,2.5]')", "X'003C004105'"),
            ("FB16_BLOB(2)", "vectorb16('[1,2.5]')", "X'803F204006'"),
            ("FLOAT8(2)", "vector8('[1,2.5]')", "X'00FF0000"),
            ("F1BIT_BLOB(2)", "vector1bit('[1,2.5]')", "X'030E03'"),
        ] {
            let c = ResultColumn::new("emb", ty);
            let k = kind_of(&c).unwrap();
            assert_eq!(
                typed_literal(Engine::Libsql, k, &val).as_deref(),
                Some(lib),
                "{ty}"
            );
            let got = typed_literal(Engine::Sqlite, k, &val).unwrap();
            assert!(got.starts_with(lite), "{ty}: {got}");
            // The blob is what libSQL itself writes, so it reads back.
            let hex = got.trim_start_matches("X'").trim_end_matches('\'');
            let back = text(&unhex(hex)).unwrap();
            let n = v::parse(&back).unwrap();
            assert_eq!(n.len(), 2, "{ty}: {back}");
        }
        // Not a vector value, or a type that isn't libSQL's: no typed literal.
        assert_eq!(
            typed_literal(Engine::Libsql, Kind::F64, &serde_json::json!("[1]")),
            None
        );
        assert_eq!(kind_of(&ResultColumn::new("emb", "vector(2)")), None);
        assert_eq!(kind_of(&ResultColumn::new("emb", "TEXT")), None);

        let cols = vec![
            ResultColumn::new("id", "INTEGER"),
            ResultColumn::new("emb", "F64_BLOB(2)"),
            // pgvector's type: the catalog decides.
            ResultColumn::new("other", "vector(2)"),
        ];
        let rows = vec![vec![serde_json::json!(1), val.clone(), val.clone()]];
        let ins = scripts::insert_typed(Engine::Libsql, &t, &cols, &rows);
        let runtime = literal(Engine::Libsql, &t, "other", &val).unwrap();
        assert_eq!(
            ins,
            format!("INSERT INTO \"docs\" (\"id\", \"emb\", \"other\") VALUES\n  (1, vector64('[1,2.5]'), {runtime});")
        );
        let change = dbine_driver::RowChange {
            key: vec![("id".into(), serde_json::json!(1))],
            set: vec![
                ("EMB".into(), serde_json::json!([0.5, 1])),
                ("other".into(), serde_json::json!([0.5, 1])),
            ],
            row: Vec::new(),
        };
        let up = scripts::update_typed(Engine::Sqlite, &t, std::slice::from_ref(&change), &cols);
        assert!(
            up.starts_with(
                "UPDATE \"docs\" SET \"EMB\" = X'000000000000E03F000000000000F03F02', \"other\" = (SELECT CASE"
            ) && up.ends_with(" WHERE \"id\" = 1;"),
            "{up}"
        );
        // Untyped: the catalog decides for every column.
        assert_eq!(
            scripts::update(Engine::Sqlite, &t, std::slice::from_ref(&change)),
            scripts::update_typed(Engine::Sqlite, &t, &[change], &[])
        );
    }
}
