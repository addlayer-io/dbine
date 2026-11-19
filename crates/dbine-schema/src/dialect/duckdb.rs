//! DuckDB (a database file, and the CSV / Parquet / JSON folder driver,
//! whose views report the same types). Rich types: unsigned and 128-bit
//! integers, lists (`T[]`), fixed arrays (`T[n]`), STRUCT, MAP, UNION,
//! ENUM, nanosecond timestamps. VARCHAR takes no length; foreign keys have
//! no CASCADE / SET NULL; auto-increment is a sequence (the driver creates
//! it).

use super::bigquery::nested;
use super::postgres::precision_loss;
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::{quote, DefaultValue};
use crate::issue::{IssueCode, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;

pub struct DuckDb;

/// What a vector without a dimension becomes (a source index that states
/// one gives it to the column before it's rendered).
const NO_DIMS: &str = "El vector no tiene dimensión fija: queda como lista; las distancias array_* y los índices HNSW de DuckDB piden un FLOAT[n].";

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: DuckDb = DuckDb;
    matches!(driver_id, "duckdb" | "duckdb_files").then_some(&D as &dyn Dialect)
}

impl Dialect for DuckDb {
    fn target_refusal(&self, driver_id: &str) -> Option<&'static str> {
        (driver_id == "duckdb_files").then_some("la conexión de archivos muestra cada archivo como una vista de solo lectura: las tablas se crean en una base DuckDB.")
    }

    fn id(&self) -> &'static str {
        "duckdb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        // Fixed-size arrays `INTEGER[3]` (lists `[]` are peeled by the parser).
        let mut raw = t.raw.trim();
        while let Some(r) = raw.strip_suffix("[]") {
            raw = r.trim_end();
        }
        if let Some(open) = raw.strip_suffix(']').and_then(|r| r.rfind('[')) {
            let size = raw[open + 1..raw.len() - 1].trim();
            if size.chars().all(|c| c.is_ascii_digit()) {
                let of = nested(self, &raw[..open]);
                // `FLOAT[n]` / `DOUBLE[n]` (not a list of them): a vector,
                // what DuckDB's array_* distances and vss's HNSW take.
                let element = match of {
                    L::Float { bytes: 4 } => Some(dbine_driver::VectorElement::F32),
                    L::Float { bytes: 8 } => Some(dbine_driver::VectorElement::F64),
                    _ => None,
                };
                if let (Some(element), Ok(dims), true) =
                    (element, size.parse::<u32>(), raw == t.raw.trim())
                {
                    return L::Vector {
                        dims: Some(dims),
                        element,
                        sparse: false,
                    };
                }
                return L::Array { of: Box::new(of) };
            }
        }
        match t.name.as_str() {
            "boolean" | "bool" | "logical" => L::Bool,
            "tinyint" | "int1" => L::int(1),
            "smallint" | "int2" | "short" => L::int(2),
            "integer" | "int4" | "int" | "signed" => L::int(4),
            "bigint" | "int8" | "long" => L::int(8),
            "hugeint" | "int128" => L::int(16),
            "utinyint" => L::Int {
                bytes: 1,
                unsigned: true,
            },
            "usmallint" => L::Int {
                bytes: 2,
                unsigned: true,
            },
            "uinteger" => L::Int {
                bytes: 4,
                unsigned: true,
            },
            "ubigint" => L::Int {
                bytes: 8,
                unsigned: true,
            },
            "uhugeint" => L::Int {
                bytes: 16,
                unsigned: true,
            },
            // Arbitrary-precision integers.
            "varint" | "bignum" => L::Decimal {
                precision: None,
                scale: Some(0),
            },
            "decimal" | "numeric" => L::Decimal {
                precision: p(0).or(Some(18)),
                scale: p(1).or(if p(0).is_some() { Some(0) } else { Some(3) }),
            },
            "real" | "float4" | "float" => L::Float { bytes: 4 },
            "double" | "float8" => L::Float { bytes: 8 },
            "varchar" | "char" | "bpchar" | "text" | "string" | "nvarchar" => {
                L::Text { unicode: true }
            }
            "blob" | "bytea" | "binary" | "varbinary" => L::Blob,
            "bit" | "bitstring" => L::Bit { len: None },
            "date" => L::Date,
            "time" => L::Time {
                precision: Some(6),
                tz: t.with_tz,
            },
            "timetz" => L::Time {
                precision: Some(6),
                tz: true,
            },
            "time_ns" => L::Time {
                precision: Some(9),
                tz: false,
            },
            "timestamp" | "datetime" => L::Timestamp {
                precision: Some(6),
                tz: t.with_tz,
            },
            "timestamp_us" => L::Timestamp {
                precision: Some(6),
                tz: false,
            },
            "timestamp_s" => L::Timestamp {
                precision: Some(0),
                tz: false,
            },
            "timestamp_ms" => L::Timestamp {
                precision: Some(3),
                tz: false,
            },
            "timestamp_ns" => L::Timestamp {
                precision: Some(9),
                tz: false,
            },
            "timestamptz" => L::Timestamp {
                precision: Some(6),
                tz: true,
            },
            "interval" => L::Interval,
            "uuid" => L::Uuid,
            "json" => L::Json { binary: false },
            "enum" => L::Enum {
                values: t.args.clone(),
            },
            "list" if t.args.len() == 1 => L::Array {
                of: Box::new(nested(self, &t.args[0])),
            },
            "map" if t.args.len() == 2 => L::Map {
                key: Box::new(nested(self, &t.args[0])),
                value: Box::new(nested(self, &t.args[1])),
            },
            // Records and tagged unions: the closest neutral type is a document.
            "struct" | "row" | "union" => L::Json { binary: true },
            "inet" => L::Inet,
            "geometry" => L::Geometry {
                kind: None,
                srid: None,
                geography: false,
            },
            _ => L::Other {
                native: t.raw.clone(),
            },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            // Text in the source's own notation (pgvector's `{1:1,3:2}/5`),
            // which a JSON column would refuse.
            L::Vector { sparse: true, .. } => Rendered::exact("VARCHAR").with(
                Warning,
                TypeApproximated,
                format!(
                    "{}: DuckDB no tiene vectores dispersos; queda como texto, en la notación del origen.",
                    t.describe()
                ),
            ),
            L::Vector { dims, element, .. } => {
                use dbine_driver::VectorElement as E;
                // Narrower numbers fit a FLOAT exactly; bits as 0 and 1.
                let of = if *element == E::F64 {
                    "DOUBLE"
                } else {
                    "FLOAT"
                };
                let r = match dims {
                    Some(n) => Rendered::exact(format!("{of}[{n}]")),
                    None => {
                        Rendered::exact(format!("{of}[]")).with(Warning, TypeApproximated, NO_DIMS)
                    }
                };
                if *element == E::Bit {
                    r.with(
                        Info,
                        TypeChanged,
                        "Vector binario: cada bit queda como un número 0 o 1.",
                    )
                } else {
                    r
                }
            }
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => {
                let (name, note) = match bytes {
                    1 => ("TINYINT", false),
                    2 => ("SMALLINT", false),
                    3 => ("INTEGER", true),
                    4 => ("INTEGER", false),
                    8 => ("BIGINT", false),
                    _ => ("HUGEINT", false),
                };
                let name = match (name, unsigned) {
                    (n, false) => n.to_string(),
                    ("INTEGER", true) => "UINTEGER".into(),
                    (n, true) => format!("U{n}"),
                };
                let r = Rendered::exact(name);
                if note {
                    r.with(Info, TypeChanged, "Entero de 3 bytes como entero de 4.")
                } else {
                    r
                }
            }
            L::Decimal {
                precision: Some(p),
                scale,
            } if *p <= 38 => Rendered::exact(format!("DECIMAL({p}, {})", scale.unwrap_or(0))),
            L::Decimal {
                precision: Some(p),
                scale,
            } => Rendered::exact(format!("DECIMAL(38, {})", scale.unwrap_or(0).min(38))).with(
                Loss,
                PrecisionLoss,
                format!("DuckDB admite hasta 38 dígitos; el origen tiene {p}."),
            ),
            L::Decimal {
                precision: None, ..
            } => Rendered::exact("DECIMAL(38, 10)").with(
                Loss,
                PrecisionLoss,
                "El origen no fija la precisión: se usa DECIMAL(38, 10).",
            ),
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(
                Info,
                TypeChanged,
                "Moneda como DECIMAL(19, 4).",
            ),
            L::Char { .. } | L::Varchar { len: Some(_), .. } => Rendered::exact("VARCHAR").with(
                Info,
                LengthLoss,
                "VARCHAR de DuckDB no limita el largo.",
            ),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact("VARCHAR"),
            L::Binary { len: Some(_) } | L::Varbinary { len: Some(_) } => {
                Rendered::exact("BLOB").with(Info, LengthLoss, "BLOB de DuckDB no limita el largo.")
            }
            L::Binary { len: None } | L::Varbinary { len: None } | L::Blob => {
                Rendered::exact("BLOB")
            }
            L::Bit { len: Some(_) } => {
                Rendered::exact("BIT").with(Info, LengthLoss, "BIT de DuckDB no fija el largo.")
            }
            L::Bit { len: None } => Rendered::exact("BIT"),
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => Rendered::exact(if *tz { "TIMETZ" } else { "TIME" })
                .with_loss(precision_loss(*precision, 6)),
            L::Timestamp {
                precision,
                tz: false,
            } => match precision {
                Some(p) if *p > 6 => {
                    Rendered::exact("TIMESTAMP_NS").with_loss(precision_loss(Some(*p), 9))
                }
                _ => Rendered::exact("TIMESTAMP"),
            },
            L::Timestamp {
                precision,
                tz: true,
            } => Rendered::exact("TIMESTAMPTZ").with_loss(precision_loss(*precision, 6)),
            L::Interval => Rendered::exact("INTERVAL"),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            L::Uuid => Rendered::exact("UUID"),
            L::Json { .. } => Rendered::exact("JSON"),
            L::Xml => Rendered::exact("VARCHAR").with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } if !values.is_empty() => Rendered::exact(format!(
                "ENUM({})",
                values
                    .iter()
                    .map(|v| quote(v))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            L::Enum { .. } => Rendered::exact("VARCHAR").with(
                Warning,
                TypeApproximated,
                "Enumerado sin valores: queda como texto.",
            ),
            L::Set { values } => Rendered::exact("VARCHAR[]").with(
                Warning,
                TypeApproximated,
                format!(
                    "Conjunto como lista de texto. Valores: {}.",
                    values.join(", ")
                ),
            ),
            L::Array { of } => {
                let inner = self.render_type(of);
                Rendered {
                    native: format!("{}[]", inner.native),
                    notes: inner.notes,
                }
            }
            L::Map { key, value } => {
                let (k, v) = (self.render_type(key), self.render_type(value));
                let mut notes = k.notes;
                notes.extend(v.notes);
                Rendered {
                    native: format!("MAP({}, {})", k.native, v.native),
                    notes,
                }
            }
            L::Geometry { .. } => Rendered::exact("VARCHAR").with(
                Warning,
                TypeApproximated,
                "Dato espacial como texto (WKT); la extensión spatial de DuckDB lo interpreta.",
            ),
            L::Inet => Rendered::exact("VARCHAR").with(
                Info,
                TypeApproximated,
                "Dirección IP como texto (el tipo INET necesita la extensión inet).",
            ),
            L::MacAddr => {
                Rendered::exact("VARCHAR").with(Info, TypeApproximated, "Dirección MAC como texto.")
            }
            L::RowVersion => Rendered::exact("UBIGINT").with(
                Warning,
                TypeApproximated,
                "DuckDB no tiene versión de fila automática.",
            ),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp if matches!(ty, L::Date) => Some("CURRENT_DATE".into()),
            DefaultValue::CurrentTime => Some("CAST(CURRENT_TIME AS TIME)".into()),
            _ => standard_default(d, ty, "CURRENT_TIMESTAMP", Some("gen_random_uuid()"), false),
        }
    }

    /// vss's HNSW: L2 (`l2sq`, its default), cosine or inner product; `m`
    /// and `ef_construction` carry over (HNSW's own), the rest is another
    /// algorithm's and doesn't. The index only takes a `FLOAT[n]` column
    /// (DuckDB refuses it on `DOUBLE[n]` and on lists): a vector of any
    /// narrower element becomes one, while a double vector keeps
    /// `DOUBLE[n]`, so as not to lose precision, and a vector that has no
    /// dimension (not even its index's, which the converter gives the
    /// column) stays a list: both lose the index, with why.
    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
        column: Option<&L>,
    ) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
        use dbine_driver::vector::{metric, param};
        use dbine_driver::VectorElement as E;
        match column {
            Some(L::Vector { sparse: true, .. }) => {
                return Err("vss solo indexa columnas FLOAT[n], y un vector disperso no lo es".into())
            }
            Some(L::Vector { dims: None, .. }) => {
                return Err("vss solo indexa columnas FLOAT[n] de dimensión fija, y la columna queda como lista sin dimensión".into())
            }
            Some(L::Vector {
                element: E::F64, ..
            }) => {
                return Err("vss solo indexa columnas FLOAT[n], y la columna queda DOUBLE[n] para no perder precisión".into())
            }
            Some(L::Vector { .. }) | None => {}
            Some(_) => return Err("vss solo indexa columnas FLOAT[n]".into()),
        }
        let metric = match v.metric.as_deref() {
            None => None,
            Some(m) if [metric::L2, metric::COSINE, metric::IP].contains(&m) => {
                Some(m.to_string())
            }
            Some(m) => {
                return Err(format!(
                    "los índices HNSW de DuckDB miden distancia L2, coseno o producto interno, y el índice usa «{m}»"
                ))
            }
        };
        let params: std::collections::BTreeMap<String, String> = v
            .params
            .iter()
            .filter(|(k, val)| {
                (k.as_str() == param::M || k.as_str() == param::EF_CONSTRUCTION)
                    && val.trim().parse::<u32>().is_ok()
            })
            .map(|(k, val)| (k.clone(), val.trim().to_string()))
            .collect();
        Ok((
            Some("HNSW".into()),
            dbine_driver::VectorIndex {
                algorithm: Some("hnsw".into()),
                metric,
                params,
            },
        ))
    }

    fn caps(&self) -> Caps {
        const ACTIONS: &[&str] = &["RESTRICT", "NO ACTION"];
        Caps {
            foreign_keys: true,
            on_delete: ACTIONS,
            on_update: ACTIONS,
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 255,
            case: IdentCase::Preserve,
            vector_indexes: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::logical_of;
    use crate::parse::parse;

    fn lt(s: &str) -> L {
        logical_of(&DuckDb, &parse(s))
    }

    #[test]
    fn vectors_round_trip() {
        use dbine_driver::VectorElement as E;
        let vec = |dims, element| L::Vector {
            dims,
            element,
            sparse: false,
        };
        assert_eq!(lt("FLOAT[3]"), vec(Some(3), E::F32));
        assert_eq!(lt("DOUBLE[768]"), vec(Some(768), E::F64));
        assert_eq!(lt("REAL[2]"), vec(Some(2), E::F32));
        // Lists, arrays of other types and lists of arrays aren't vectors.
        assert_eq!(
            lt("FLOAT[]"),
            L::Array {
                of: Box::new(L::Float { bytes: 4 })
            }
        );
        assert!(matches!(lt("INTEGER[3]"), L::Array { .. }));
        assert!(matches!(lt("FLOAT[3][]"), L::Array { .. }));
        for (t, native) in [
            (vec(Some(3), E::F32), "FLOAT[3]"),
            (vec(Some(2), E::F64), "DOUBLE[2]"),
            // pgvector's halfvec(4) and bit(8), as their dialect reads them.
            (vec(Some(4), E::F16), "FLOAT[4]"),
            (vec(Some(8), E::Bit), "FLOAT[8]"),
            (vec(None, E::F32), "FLOAT[]"),
        ] {
            assert_eq!(DuckDb.render_type(&t).native, native, "{t:?}");
        }
        assert!(DuckDb.render_type(&vec(None, E::F32)).notes.len() == 1);
        // A sparse vector as text in its own notation, not JSON.
        let r = DuckDb.render_type(&L::Vector {
            dims: Some(5),
            element: E::F32,
            sparse: true,
        });
        assert_eq!(r.native, "VARCHAR");
        assert!(r.notes[0].message.contains("dispersos"), "{:?}", r.notes);
    }

    #[test]
    fn vector_indexes_become_hnsw() {
        use dbine_driver::VectorIndex;
        let v = VectorIndex {
            algorithm: Some("diskann".into()),
            metric: Some("cosine".into()),
            params: [
                ("m", "20"),
                ("quantization", "float8"),
                ("ef_construction", "x"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        };
        let (kind, out) = DuckDb.vector_index(&v, None).unwrap();
        assert_eq!(kind.as_deref(), Some("HNSW"));
        assert_eq!(out.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(out.metric.as_deref(), Some("cosine"));
        assert_eq!(
            out.params.into_iter().collect::<Vec<_>>(),
            [("m".to_string(), "20".to_string())]
        );
        let e = DuckDb
            .vector_index(
                &VectorIndex {
                    metric: Some("hamming".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap_err();
        assert!(e.contains("hamming"), "{e}");
        // The column it searches decides: FLOAT[n] takes it (any element
        // narrower than a double becomes FLOAT), the rest don't.
        use dbine_driver::VectorElement as E;
        let vec = |dims, element, sparse| L::Vector {
            dims,
            element,
            sparse,
        };
        for e in [E::F32, E::F16, E::Bf16, E::I8, E::Bit] {
            assert!(DuckDb
                .vector_index(&v, Some(&vec(Some(3), e, false)))
                .is_ok());
        }
        for (col, why) in [
            (vec(Some(3), E::F64, false), "DOUBLE[n]"),
            (vec(None, E::F32, false), "sin dimensión"),
            (vec(Some(3), E::F32, true), "disperso"),
            (L::Text { unicode: true }, "FLOAT[n]"),
        ] {
            let e = DuckDb.vector_index(&v, Some(&col)).unwrap_err();
            assert!(e.contains(why), "{e}");
        }
    }

    /// Into DuckDB, vss's HNSW needs a `FLOAT[n]` column: a ClickHouse
    /// `Array(Float32)` takes its index's dimension; a double vector keeps
    /// its precision and loses the index, with why (DuckDB refuses HNSW on
    /// `DOUBLE[n]` and on lists).
    #[test]
    fn vector_indexes_into_duckdb() {
        use super::super::clickhouse::tests::vector_table;
        use crate::convert::{convert, Options};
        let one = |t, from: &str| {
            let r = convert(&[t], from, "duckdb", &Options::default()).unwrap();
            (r.tables[0].clone(), r.issues)
        };
        let ch = |ty, params: &[(&str, &str)]| vector_table(ty, Some("vector_similarity"), params);
        let (t, issues) = one(
            ch("Array(Float32)", &[("dimensions", "3"), ("m", "8")]),
            "clickhouse",
        );
        assert_eq!(t.columns[1].data_type, "FLOAT[3]");
        let v = t.indexes[0].vector.as_ref().unwrap();
        assert_eq!(t.indexes[0].kind.as_deref(), Some("HNSW"));
        assert_eq!(
            v.params.iter().collect::<Vec<_>>(),
            [(&"m".to_string(), &"8".to_string())],
            "the dimension went to the column"
        );
        // The converter takes the index's dimension for the column, so the
        // array arrives as FLOAT[3] with no change to report.
        // The render's "no dimension" warning is taken back.
        assert!(!issues.iter().any(|i| i.message == NO_DIMS), "{issues:?}");
        let (t, _) = one(ch("Array(BFloat16)", &[("dimensions", "4")]), "clickhouse");
        assert_eq!(t.columns[1].data_type, "FLOAT[4]");
        assert_eq!(t.indexes.len(), 1);
        for (t, from, ty) in [
            (
                ch("Array(Float64)", &[("dimensions", "3")]),
                "clickhouse",
                "DOUBLE[3]",
            ),
            (
                vector_table("F64_BLOB(3)", None, &[]),
                "libsql",
                "DOUBLE[3]",
            ),
            // ClickHouse 24.8–25.7 indexes say no dimension.
            (ch("Array(Float32)", &[]), "clickhouse", "FLOAT[]"),
        ] {
            let (t, issues) = one(t, from);
            assert_eq!(t.columns[1].data_type, ty);
            assert!(t.indexes.is_empty(), "{ty}");
            assert!(
                issues.iter().any(|i| i.code == IssueCode::IndexDropped
                    && i.message.contains("vss solo indexa columnas FLOAT[n]")),
                "{issues:?}"
            );
        }
    }

    /// DuckDB → libSQL → DuckDB: the vector type and its index cross
    /// families (pgvector's `vector(3)` reads as the same logical type).
    #[test]
    fn vectors_cross_to_libsql_and_back() {
        use crate::convert::{convert, Options};
        use dbine_driver::{ColumnDef, IndexDef, TableSchema, VectorIndex};
        let t = TableSchema {
            name: "docs".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: "INTEGER".into(),
                    ..Default::default()
                },
                ColumnDef {
                    name: "emb".into(),
                    data_type: "FLOAT[3]".into(),
                    nullable: true,
                    ..Default::default()
                },
            ],
            indexes: vec![IndexDef {
                name: "docs_emb".into(),
                columns: vec!["emb".into()],
                kind: Some("HNSW".into()),
                vector: Some(VectorIndex {
                    algorithm: Some("hnsw".into()),
                    metric: Some("l2".into()),
                    params: [("m".to_string(), "16".to_string())].into(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let r = convert(&[t], "duckdb", "libsql", &Options::default()).unwrap();
        let lt = &r.tables[0];
        assert_eq!(lt.columns[1].data_type, "F32_BLOB(3)");
        let ix = &lt.indexes[0];
        assert_eq!(ix.kind, None);
        let v = ix.vector.as_ref().unwrap();
        assert_eq!(
            (v.algorithm.as_deref(), v.metric.as_deref()),
            (None, Some("l2"))
        );
        assert_eq!(v.params["m"], "16");
        let back = convert(&r.tables, "libsql", "duckdb", &Options::default()).unwrap();
        let dt = &back.tables[0];
        assert_eq!(dt.columns[1].data_type, "FLOAT[3]");
        assert_eq!(dt.indexes[0].kind.as_deref(), Some("HNSW"));
        assert_eq!(
            dt.indexes[0].vector.as_ref().unwrap().metric.as_deref(),
            Some("l2")
        );
    }

    #[test]
    fn parses_catalog_spellings() {
        assert_eq!(lt("INTEGER"), L::int(4));
        assert_eq!(lt("HUGEINT"), L::int(16));
        assert_eq!(
            lt("UBIGINT"),
            L::Int {
                bytes: 8,
                unsigned: true
            }
        );
        assert_eq!(
            lt("UHUGEINT"),
            L::Int {
                bytes: 16,
                unsigned: true
            }
        );
        assert_eq!(
            lt("DECIMAL(18,3)"),
            L::Decimal {
                precision: Some(18),
                scale: Some(3)
            }
        );
        assert_eq!(lt("FLOAT"), L::Float { bytes: 4 });
        assert_eq!(lt("DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(lt("VARCHAR"), L::Text { unicode: true });
        assert_eq!(lt("BLOB"), L::Blob);
        assert_eq!(lt("BIT"), L::Bit { len: None });
        assert_eq!(
            lt("TIME WITH TIME ZONE"),
            L::Time {
                precision: Some(6),
                tz: true
            }
        );
        assert_eq!(
            lt("TIMESTAMP WITH TIME ZONE"),
            L::Timestamp {
                precision: Some(6),
                tz: true
            }
        );
        assert_eq!(
            lt("TIMESTAMP_NS"),
            L::Timestamp {
                precision: Some(9),
                tz: false
            }
        );
        assert_eq!(
            lt("TIMESTAMP_S"),
            L::Timestamp {
                precision: Some(0),
                tz: false
            }
        );
        assert_eq!(lt("INTERVAL"), L::Interval);
        assert_eq!(lt("UUID"), L::Uuid);
        assert_eq!(lt("JSON"), L::Json { binary: false });
        assert_eq!(
            lt("ENUM('a', 'b')"),
            L::Enum {
                values: vec!["a".into(), "b".into()]
            }
        );
        assert_eq!(
            lt("INTEGER[]"),
            L::Array {
                of: Box::new(L::int(4))
            }
        );
        assert_eq!(
            lt("VARCHAR[][]"),
            L::Array {
                of: Box::new(L::Array {
                    of: Box::new(L::Text { unicode: true })
                })
            }
        );
        // A fixed-size array of floats is a vector (see `vectors_round_trip`).
        assert_eq!(
            lt("SMALLINT[3]"),
            L::Array {
                of: Box::new(L::int(2))
            }
        );
        assert_eq!(
            lt("MAP(VARCHAR, INTEGER)"),
            L::Map {
                key: Box::new(L::Text { unicode: true }),
                value: Box::new(L::int(4))
            }
        );
        assert_eq!(lt("STRUCT(a INTEGER, b VARCHAR)"), L::Json { binary: true });
        assert_eq!(
            lt("VARINT"),
            L::Decimal {
                precision: None,
                scale: Some(0)
            }
        );
        assert!(matches!(lt("SOMETHING_ELSE"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| DuckDb.render_type(&t);
        assert_eq!(r(L::Bool).native, "BOOLEAN");
        assert_eq!(
            r(L::Int {
                bytes: 1,
                unsigned: true
            })
            .native,
            "UTINYINT"
        );
        assert_eq!(
            r(L::Int {
                bytes: 4,
                unsigned: true
            })
            .native,
            "UINTEGER"
        );
        assert_eq!(
            r(L::Int {
                bytes: 8,
                unsigned: true
            })
            .native,
            "UBIGINT"
        );
        assert_eq!(r(L::int(3)).native, "INTEGER");
        assert_eq!(r(L::int(16)).native, "HUGEINT");
        assert_eq!(
            r(L::Decimal {
                precision: Some(12),
                scale: Some(2)
            })
            .native,
            "DECIMAL(12, 2)"
        );
        assert_eq!(
            r(L::Decimal {
                precision: Some(65),
                scale: Some(30)
            })
            .native,
            "DECIMAL(38, 30)"
        );
        assert_eq!(
            r(L::Decimal {
                precision: None,
                scale: None
            })
            .native,
            "DECIMAL(38, 10)"
        );
        assert_eq!(r(L::Float { bytes: 4 }).native, "FLOAT");
        assert_eq!(r(L::Float { bytes: 8 }).native, "DOUBLE");
        assert_eq!(r(L::Money).native, "DECIMAL(19, 4)");
        assert_eq!(
            r(L::Char {
                len: Some(3),
                unicode: true
            })
            .native,
            "VARCHAR"
        );
        assert_eq!(
            r(L::Varchar {
                len: Some(30),
                unicode: true
            })
            .native,
            "VARCHAR"
        );
        assert_eq!(r(L::Text { unicode: false }).native, "VARCHAR");
        assert_eq!(r(L::Binary { len: Some(16) }).native, "BLOB");
        assert_eq!(r(L::Blob).native, "BLOB");
        assert_eq!(r(L::Bit { len: Some(8) }).native, "BIT");
        assert_eq!(r(L::Date).native, "DATE");
        assert_eq!(
            r(L::Time {
                precision: None,
                tz: true
            })
            .native,
            "TIMETZ"
        );
        assert_eq!(
            r(L::Timestamp {
                precision: Some(3),
                tz: false
            })
            .native,
            "TIMESTAMP"
        );
        assert_eq!(
            r(L::Timestamp {
                precision: Some(7),
                tz: false
            })
            .native,
            "TIMESTAMP_NS"
        );
        assert_eq!(
            r(L::Timestamp {
                precision: Some(7),
                tz: true
            })
            .notes[0]
                .code,
            IssueCode::PrecisionLoss
        );
        assert_eq!(r(L::Interval).native, "INTERVAL");
        assert_eq!(r(L::Year).native, "SMALLINT");
        assert_eq!(r(L::Uuid).native, "UUID");
        assert_eq!(r(L::Json { binary: true }).native, "JSON");
        assert_eq!(r(L::Xml).native, "VARCHAR");
        assert_eq!(
            r(L::Enum {
                values: vec!["a".into(), "b".into()]
            })
            .native,
            "ENUM('a', 'b')"
        );
        assert_eq!(
            r(L::Set {
                values: vec!["a".into()]
            })
            .native,
            "VARCHAR[]"
        );
        assert_eq!(
            r(L::Array {
                of: Box::new(L::int(4))
            })
            .native,
            "INTEGER[]"
        );
        assert_eq!(
            r(L::Map {
                key: Box::new(L::Text { unicode: true }),
                value: Box::new(L::int(8))
            })
            .native,
            "MAP(VARCHAR, BIGINT)"
        );
        assert_eq!(
            r(L::Geometry {
                kind: None,
                srid: None,
                geography: false
            })
            .native,
            "VARCHAR"
        );
        assert_eq!(r(L::Inet).native, "VARCHAR");
        assert_eq!(r(L::MacAddr).native, "VARCHAR");
        assert_eq!(r(L::RowVersion).native, "UBIGINT");
    }

    #[test]
    fn defaults() {
        let d = DuckDb;
        let ts = L::Timestamp {
            precision: None,
            tz: true,
        };
        assert_eq!(
            d.render_default(&DefaultValue::CurrentTimestamp, &ts)
                .as_deref(),
            Some("CURRENT_TIMESTAMP")
        );
        assert_eq!(
            d.render_default(&DefaultValue::NewUuid, &L::Uuid)
                .as_deref(),
            Some("gen_random_uuid()")
        );
        assert_eq!(
            d.render_default(&DefaultValue::Bool(true), &L::Bool)
                .as_deref(),
            Some("TRUE")
        );
        assert_eq!(
            d.render_default(
                &DefaultValue::Text("it's".into()),
                &L::Text { unicode: true }
            )
            .as_deref(),
            Some("'it''s'")
        );
    }
}
