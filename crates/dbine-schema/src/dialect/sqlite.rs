//! SQLite (and libSQL). SQLite stores values by *affinity*, not by the
//! declared type, and doesn't enforce lengths: parsing keeps the declared
//! intent (a `DATETIME` column holds dates), rendering keeps names other
//! engines and people recognize (and sizes, so a round trip through SQLite
//! gives the same types back).
//!
//! Types arrive as declared in CREATE TABLE (`pragma_table_xinfo.type`),
//! any case and spelling: `VARCHAR(20)`, `UNSIGNED BIG INT`, `NUMERIC(10, 2)`.

use super::postgres::{uuid_slot, UuidSlot};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered, ALL_ACTIONS};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{parse, TypeSpec};
use dbine_driver::TableSchema;

pub struct Sqlite;

/// What a vector without a dimension becomes (a source index that states
/// one gives it to the column before it's rendered).
const NO_DIMS: &str = "El vector no tiene dimensión fija: libSQL no crea índices vectoriales sobre una columna sin dimensión.";

impl Dialect for Sqlite {
    fn id(&self) -> &'static str {
        "sqlite"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        // `UNSIGNED BIG INT` starts with a modifier: the whole spelling decides.
        let raw = t.raw.to_ascii_lowercase();
        let n = if t.name.is_empty() && !raw.is_empty() {
            raw.as_str()
        } else {
            t.name.as_str()
        };
        let int = |bytes| L::Int {
            bytes,
            unsigned: t.unsigned,
        };
        if let Some(v) = libsql_vector(t) {
            return v;
        }
        match n {
            "boolean" | "bool" => L::Bool,
            "tinyint" => int(1),
            "smallint" | "int2" => int(2),
            "mediumint" => int(3),
            "int" | "int4" => int(4),
            // INTEGER is SQLite's 8-byte integer (the rowid).
            "integer" | "bigint" | "int8" => int(8),
            "unsigned big int" => L::Int {
                bytes: 8,
                unsigned: true,
            },
            "real" | "double" | "double precision" | "float" => L::Float { bytes: 8 },
            "numeric" | "decimal" => L::Decimal {
                precision: p(0),
                scale: p(1).or(p(0).map(|_| 0)),
            },
            "char" | "character" | "nchar" | "native character" => L::Char {
                len: p(0),
                unicode: true,
            },
            "varchar" | "nvarchar" | "varying character" | "character varying" => L::Varchar {
                len: p(0),
                unicode: true,
            },
            "text" | "clob" => L::Text { unicode: true },
            "blob" => L::Blob,
            // No type: BLOB affinity, stores anything as given.
            "" => L::Blob,
            "date" => L::Date,
            "time" => L::Time {
                precision: None,
                tz: false,
            },
            "datetime" | "timestamp" => L::Timestamp {
                precision: None,
                tz: false,
            },
            "uuid" => L::Uuid,
            "json" => L::Json { binary: false },
            // Affinity rules (https://sqlite.org/datatype3.html §3.1).
            _ if n.contains("int") => int(8),
            _ if n.contains("char") || n.contains("clob") || n.contains("text") => {
                L::Text { unicode: true }
            }
            _ if n.contains("blob") => L::Blob,
            _ if n.contains("real") || n.contains("floa") || n.contains("doub") => {
                L::Float { bytes: 8 }
            }
            _ => L::Other {
                native: t.raw.clone(),
            },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Vector { sparse: true, .. } => super::vector_fallback(self, t),
            L::Vector { dims, element, .. } => {
                use dbine_driver::VectorElement as E;
                let name = match element {
                    E::F64 => "F64_BLOB",
                    E::F16 => "F16_BLOB",
                    E::Bf16 => "FB16_BLOB",
                    E::Bit => "F1BIT_BLOB",
                    E::F32 | E::I8 | E::U8 | E::Other => "F32_BLOB",
                };
                match dims {
                    Some(n) => Rendered::exact(format!("{name}({n})")),
                    None => Rendered::exact(name).with(Warning, TypeApproximated, NO_DIMS),
                }
            }
            L::Bool => Rendered::exact("BOOLEAN"),
            // All are 8-byte INTEGER affinity; the name keeps the size.
            L::Int { bytes, unsigned } => match L::signed_bytes_for(*bytes, *unsigned) {
                1 => Rendered::exact("TINYINT"),
                2 => Rendered::exact("SMALLINT"),
                3 => Rendered::exact("MEDIUMINT"),
                4 => Rendered::exact("INT"),
                8 => Rendered::exact("BIGINT"),
                _ => Rendered::exact("NUMERIC").with(
                    Loss,
                    RangeLoss,
                    "SQLite guarda enteros de hasta 8 bytes; los mayores quedan como número real.",
                ),
            },
            L::Decimal { precision, scale } => {
                let native = match (precision, scale) {
                    (Some(p), Some(s)) => format!("NUMERIC({p}, {s})"),
                    (Some(p), None) => format!("NUMERIC({p})"),
                    _ => "NUMERIC".into(),
                };
                let r = Rendered::exact(native);
                if precision.is_none_or(|p| p > 15) || scale.is_some_and(|s| s > 0) {
                    r.with(Loss, PrecisionLoss, "SQLite guarda los decimales como número real (unos 15 dígitos): puede redondear.")
                } else {
                    r
                }
            }
            L::Float { .. } => Rendered::exact("REAL"),
            L::Money => Rendered::exact("NUMERIC").with(
                Loss,
                PrecisionLoss,
                "SQLite guarda la moneda como número real: puede redondear.",
            ),
            L::Char { len, .. } => {
                Rendered::exact(len.map_or("CHAR".into(), |n| format!("CHAR({n})")))
            }
            L::Varchar { len: Some(n), .. } => Rendered::exact(format!("VARCHAR({n})")),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact("TEXT"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("BLOB"),
            L::Bit { .. } => Rendered::exact("BLOB").with(
                Warning,
                TypeApproximated,
                "SQLite no tiene cadenas de bits: se guarda como binario.",
            ),
            L::Date => Rendered::exact("DATE"),
            L::Time { tz, .. } => {
                let r = Rendered::exact("TIME");
                if *tz {
                    r.with(Loss, TimeZoneLoss, "SQLite no guarda zonas horarias.")
                } else {
                    r
                }
            }
            L::Timestamp { tz, .. } => {
                let r = Rendered::exact("DATETIME");
                if *tz {
                    r.with(
                        Loss,
                        TimeZoneLoss,
                        "SQLite no guarda zonas horarias: conviene guardar en UTC.",
                    )
                } else {
                    r
                }
            }
            L::Interval => {
                Rendered::exact("TEXT").with(Warning, TypeApproximated, "Intervalo como texto.")
            }
            L::Year => Rendered::exact("INTEGER"),
            L::Uuid => Rendered::exact("TEXT").with(Info, TypeChanged, "UUID como texto."),
            L::Json { .. } => Rendered::exact("TEXT").with(
                Info,
                TypeChanged,
                "JSON como texto (las funciones json_* de SQLite lo leen).",
            ),
            L::Xml => Rendered::exact("TEXT"),
            L::Enum { values } | L::Set { values } => Rendered::exact("TEXT").with(
                Warning,
                TypeApproximated,
                format!(
                    "SQLite no tiene enumerados: queda como texto. Valores: {}.",
                    values.join(", ")
                ),
            ),
            L::Array { .. } | L::Map { .. } => Rendered::exact("TEXT").with(
                Warning,
                TypeApproximated,
                "SQLite no tiene arreglos ni mapas: se guarda como JSON en texto.",
            ),
            L::Geometry { .. } => Rendered::exact("BLOB").with(
                Warning,
                TypeApproximated,
                "Dato espacial como binario (SpatiaLite lo interpreta).",
            ),
            L::Inet | L::MacAddr => Rendered::exact("TEXT"),
            L::RowVersion => Rendered::exact("BLOB").with(
                Warning,
                TypeApproximated,
                "SQLite no tiene versión de fila automática.",
            ),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            // No UUID function: a random version-4 one from randomblob().
            DefaultValue::NewUuid => match uuid_slot(ty) {
                UuidSlot::Native | UuidSlot::Text => Some(
                    "(lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || \
                     substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6))))"
                        .into(),
                ),
                UuidSlot::Binary => Some("(randomblob(16))".into()),
                UuidSlot::None => None,
            },
            DefaultValue::CurrentTimestamp if matches!(ty, L::Date) => Some("CURRENT_DATE".into()),
            DefaultValue::CurrentTimestamp if matches!(ty, L::Time { .. }) => Some("CURRENT_TIME".into()),
            _ => standard_default(d, ty, "CURRENT_TIMESTAMP", None, true),
        }
    }

    /// libSQL indexes a dense vector column with a dimension, in any of
    /// its formats (`F32_BLOB(n)` to `F1BIT_BLOB(n)`, checked on sqld):
    /// the converter gives the column the dimension its source index
    /// states (a ClickHouse array), so a vector that still has none, or a
    /// sparse one (kept as text), can't be indexed.
    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
        column: Option<&L>,
    ) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
        match column {
            Some(L::Vector { sparse: true, .. }) => {
                return Err("libSQL no indexa vectores dispersos, que quedan como texto".into())
            }
            Some(L::Vector { dims: None, .. }) => {
                return Err("libSQL no indexa una columna vectorial sin dimensión, y ni el índice ni la columna del origen la dan".into())
            }
            Some(L::Vector { .. }) | None => {}
            Some(_) => {
                return Err("libSQL lo crea sobre columnas vectoriales (F32_BLOB(n), F64_BLOB(n)…)".into())
            }
        }
        libsql_vector_index(v)
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            on_delete: ALL_ACTIONS,
            on_update: ALL_ACTIONS,
            indexes: true,
            partial_indexes: true,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: false,
            max_identifier: 1024,
            case: IdentCase::Preserve,
            vector_indexes: true,
        }
    }

    /// AUTOINCREMENT only exists on an `INTEGER PRIMARY KEY` of one column:
    /// an integer key keeps it as `INTEGER` (SQLite's integers are all 8
    /// bytes); anything else loses it.
    fn finalize(&self, t: &mut TableSchema, report: &mut crate::issue::Report) {
        let pk: Vec<String> = t
            .primary_key
            .as_ref()
            .map(|k| k.columns.clone())
            .unwrap_or_default();
        for c in &mut t.columns {
            if !c.auto_increment {
                continue;
            }
            // A key of any whole-number type counts (MySQL's BIGINT UNSIGNED,
            // Oracle's NUMBER(19) / INTEGER): the rowid goes up to 2^63 - 1.
            let logical = self.parse_type(&parse(&c.data_type));
            let whole = matches!(
                logical,
                L::Int { .. }
                    | L::Decimal {
                        scale: Some(0) | None,
                        ..
                    }
            );
            if pk.len() == 1 && pk[0] == c.name && whole {
                if !matches!(
                    logical,
                    L::Int {
                        bytes: 1..=8,
                        unsigned: false
                    }
                ) {
                    report.push(
                        Severity::Loss,
                        IssueCode::RangeLoss,
                        &t.name,
                        Some(&c.name),
                        format!("La clave autoincremental de SQLite es INTEGER (hasta 2^63 - 1): «{}» pasa a INTEGER.", c.data_type),
                    );
                }
                c.data_type = "INTEGER".into();
            } else {
                c.auto_increment = false;
                report.push(
                    Severity::Loss,
                    IssueCode::AutoIncrementDropped,
                    &t.name,
                    Some(&c.name),
                    "En SQLite solo una clave primaria INTEGER de una columna es autoincremental.",
                );
            }
        }
    }
}

/// libSQL's vector column types: `F32_BLOB(n)` (`FLOAT32(n)`), `F64_BLOB`,
/// `F16_BLOB`, `FB16_BLOB`, `F8_BLOB` and `F1BIT_BLOB`. The `FLOATnn`
/// spellings only with a dimension: a bare `FLOAT64` is a REAL. `F8` is
/// libSQL's own scaled bytes, read as floats: its element is `Other`.
fn libsql_vector(t: &TypeSpec) -> Option<L> {
    use dbine_driver::VectorElement as E;
    let dims = t.arg_u32(0);
    let (element, blob) = match t.name.as_str() {
        "f32_blob" => (E::F32, true),
        "f64_blob" => (E::F64, true),
        "f16_blob" => (E::F16, true),
        "fb16_blob" => (E::Bf16, true),
        "f8_blob" => (E::Other, true),
        "f1bit_blob" => (E::Bit, true),
        "float32" => (E::F32, false),
        "float64" => (E::F64, false),
        "float16" => (E::F16, false),
        "floatb16" => (E::Bf16, false),
        "float8" => (E::Other, false),
        "float1bit" => (E::Bit, false),
        _ => return None,
    };
    (blob || dims.is_some()).then_some(L::Vector {
        dims,
        element,
        sparse: false,
    })
}

/// A vector index of another family as libSQL's (DiskANN through
/// `libsql_vector_idx`): cosine or L2 distance; `m` (`max_neighbors`),
/// `ef_construction` (`insert_l`) and `quantization`
/// (`compress_neighbors`) carry over, the other parameters are the source
/// algorithm's and don't. The driver writes the libSQL names.
fn libsql_vector_index(
    v: &dbine_driver::VectorIndex,
) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
    use dbine_driver::vector::{metric, param};
    let metric = match v.metric.as_deref() {
        None => None,
        Some(m) if m == metric::COSINE || m == metric::L2 => Some(m.to_string()),
        Some(m) => {
            return Err(format!(
                "libSQL solo indexa con distancia coseno o L2, y el índice usa «{m}»"
            ))
        }
    };
    let mut params = std::collections::BTreeMap::new();
    for (k, val) in &v.params {
        match k.as_str() {
            param::M | param::EF_CONSTRUCTION if val.trim().parse::<u32>().is_ok() => {
                params.insert(k.clone(), val.trim().to_string());
            }
            param::QUANTIZATION => {
                let q = match val.trim().to_ascii_lowercase().as_str() {
                    "binary" | "bit" | "b1" | "float1bit" => Some("float1bit"),
                    "int8" | "i8" | "float8" => Some("float8"),
                    "f16" | "half" | "halfvec" | "float16" => Some("float16"),
                    "bf16" | "floatb16" => Some("floatb16"),
                    "f32" | "float32" | "none" => Some("float32"),
                    _ => None,
                };
                if let Some(q) = q {
                    params.insert(k.clone(), q.to_string());
                }
            }
            _ => {}
        }
    }
    Ok((
        None,
        dbine_driver::VectorIndex {
            algorithm: None,
            metric,
            params,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(s: &str) -> L {
        Sqlite.parse_type(&parse(s))
    }

    #[test]
    fn libsql_vectors_round_trip() {
        use dbine_driver::VectorElement as E;
        let vec = |dims, element| L::Vector {
            dims,
            element,
            sparse: false,
        };
        for (native, want) in [
            ("F32_BLOB(3)", vec(Some(3), E::F32)),
            ("F64_BLOB(768)", vec(Some(768), E::F64)),
            ("F16_BLOB(2)", vec(Some(2), E::F16)),
            ("FB16_BLOB(2)", vec(Some(2), E::Bf16)),
            ("F1BIT_BLOB(64)", vec(Some(64), E::Bit)),
            ("F32_BLOB", vec(None, E::F32)),
        ] {
            assert_eq!(ty(native), want, "{native}");
            assert_eq!(Sqlite.render_type(&want).native, native);
        }
        assert_eq!(ty("float32(4)"), vec(Some(4), E::F32));
        assert_eq!(ty("F8_BLOB(4)"), vec(Some(4), E::Other));
        // A bare FLOAT64 is a REAL.
        assert_eq!(ty("FLOAT64"), L::Float { bytes: 8 });
        // pgvector's vector(3) and halfvec(3), as the PostgreSQL dialect
        // reads them; a sparse vector goes as JSON text.
        assert_eq!(
            Sqlite.render_type(&vec(Some(3), E::F32)).native,
            "F32_BLOB(3)"
        );
        assert_eq!(
            Sqlite.render_type(&vec(Some(3), E::F16)).native,
            "F16_BLOB(3)"
        );
        assert!(!Sqlite.render_type(&vec(None, E::F32)).notes.is_empty());
        let sparse = Sqlite.render_type(&L::Vector {
            dims: Some(5),
            element: E::F32,
            sparse: true,
        });
        assert_eq!(sparse.native, "TEXT");
    }

    /// Into libSQL a vector index needs a dense vector column with a
    /// dimension, in any format: a ClickHouse array takes its index's
    /// (an `Array(Float64)` one becomes an `F64_BLOB(n)`, which libSQL
    /// indexes too); a column that isn't a vector, or has no dimension
    /// anywhere, loses the index, with why.
    #[test]
    fn vector_indexes_into_libsql() {
        use super::super::clickhouse::tests::vector_table;
        use crate::convert::{convert, Options};
        let one = |t, from: &str| {
            let r = convert(&[t], from, "libsql", &Options::default()).unwrap();
            (r.tables[0].clone(), r.issues)
        };
        let ch = |ty, params: &[(&str, &str)]| vector_table(ty, Some("vector_similarity"), params);
        let (t, issues) = one(
            ch("Array(Float32)", &[("dimensions", "3"), ("m", "8")]),
            "clickhouse",
        );
        assert_eq!(t.columns[1].data_type, "F32_BLOB(3)");
        assert!(!issues.iter().any(|i| i.message == NO_DIMS), "{issues:?}");
        let v = t.indexes[0].vector.as_ref().unwrap();
        assert_eq!(
            v.params.iter().collect::<Vec<_>>(),
            [(&"m".to_string(), &"8".to_string())],
            "the dimension is the column's, not a libSQL setting"
        );
        for (ty, native) in [
            ("Array(BFloat16)", "FB16_BLOB(2)"),
            ("Array(Float64)", "F64_BLOB(2)"),
        ] {
            let (t, issues) = one(ch(ty, &[("dimensions", "2")]), "clickhouse");
            assert_eq!(t.columns[1].data_type, native);
            assert_eq!(t.indexes.len(), 1, "{issues:?}");
        }
        for (t, why) in [
            (ch("Array(Float32)", &[]), "sin dimensión"),
            (
                ch("Array(Int64)", &[("dimensions", "3")]),
                "no es un vector",
            ),
            (
                vector_table("VARCHAR", Some("HNSW"), &[("dims", "3")]),
                "no es un vector",
            ),
        ] {
            let (t, issues) = one(t, "clickhouse");
            assert!(t.indexes.is_empty());
            assert!(
                issues
                    .iter()
                    .any(|i| i.code == IssueCode::IndexDropped && i.message.contains(why)),
                "{issues:?}"
            );
        }
        // DuckDB's FLOAT[3] and DOUBLE[3] already have their dimension.
        for (ty, native) in [("FLOAT[3]", "F32_BLOB(3)"), ("DOUBLE[3]", "F64_BLOB(3)")] {
            let (t, _) = one(vector_table(ty, Some("HNSW"), &[]), "duckdb");
            assert_eq!(t.columns[1].data_type, native);
            assert_eq!(t.indexes.len(), 1);
        }
        // A sparse vector (pgvector's sparsevec) stays text, without index.
        let (t, issues) = one(vector_table("sparsevec(5)", Some("hnsw"), &[]), "postgres");
        assert_eq!(t.columns[1].data_type, "TEXT");
        assert!(t.indexes.is_empty());
        assert!(
            issues
                .iter()
                .any(|i| i.code == IssueCode::IndexDropped && i.message.contains("dispersos")),
            "{issues:?}"
        );
    }

    #[test]
    fn vector_index_needs_a_dense_vector_with_a_dimension() {
        use dbine_driver::{VectorElement as E, VectorIndex};
        let v = VectorIndex {
            metric: Some("l2".into()),
            ..Default::default()
        };
        let vec = |dims, element, sparse| L::Vector {
            dims,
            element,
            sparse,
        };
        for e in [E::F32, E::F64, E::F16, E::Bf16, E::I8, E::Bit] {
            assert!(Sqlite
                .vector_index(&v, Some(&vec(Some(4), e, false)))
                .is_ok());
        }
        assert!(Sqlite.vector_index(&v, None).is_ok());
        for (col, why) in [
            (vec(None, E::F32, false), "sin dimensión"),
            (vec(Some(4), E::F32, true), "dispersos"),
            (L::Text { unicode: true }, "columnas vectoriales"),
        ] {
            let e = Sqlite.vector_index(&v, Some(&col)).unwrap_err();
            assert!(e.contains(why), "{e}");
        }
    }

    #[test]
    fn vector_indexes_become_libsql_ones() {
        use dbine_driver::VectorIndex;
        let ix = |metric: &str, params: &[(&str, &str)]| VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some(metric.into()),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let (kind, v) = Sqlite
            .vector_index(
                &ix(
                    "cosine",
                    &[("m", "16"), ("ef_construction", "64"), ("lists", "100")],
                ),
                None,
            )
            .unwrap();
        assert_eq!(kind, None);
        assert_eq!(v.algorithm, None, "DiskANN, libSQL's only one");
        assert_eq!(v.metric.as_deref(), Some("cosine"));
        assert_eq!(
            v.params.into_iter().collect::<Vec<_>>(),
            [
                ("ef_construction".to_string(), "64".to_string()),
                ("m".into(), "16".into())
            ]
        );
        let (_, v) = Sqlite
            .vector_index(&ix("l2", &[("quantization", "int8")]), None)
            .unwrap();
        assert_eq!(v.params["quantization"], "float8");
        let e = Sqlite.vector_index(&ix("ip", &[]), None).unwrap_err();
        assert!(e.contains("coseno o L2"), "{e}");
    }

    #[test]
    fn parses_declared_types() {
        assert_eq!(ty("INTEGER"), L::int(8));
        assert_eq!(ty("INT"), L::int(4));
        assert_eq!(
            ty("UNSIGNED BIG INT"),
            L::Int {
                bytes: 8,
                unsigned: true
            }
        );
        assert_eq!(
            ty("INT UNSIGNED"),
            L::Int {
                bytes: 4,
                unsigned: true
            }
        );
        assert_eq!(
            ty("NUMERIC(10, 2)"),
            L::Decimal {
                precision: Some(10),
                scale: Some(2)
            }
        );
        assert_eq!(
            ty("DECIMAL(10)"),
            L::Decimal {
                precision: Some(10),
                scale: Some(0)
            }
        );
        assert_eq!(
            ty("VARCHAR(20)"),
            L::Varchar {
                len: Some(20),
                unicode: true
            }
        );
        assert_eq!(
            ty("NATIVE CHARACTER(70)"),
            L::Char {
                len: Some(70),
                unicode: true
            }
        );
        assert_eq!(
            ty("DATETIME"),
            L::Timestamp {
                precision: None,
                tz: false
            }
        );
        assert_eq!(ty(""), L::Blob);
        assert_eq!(
            ty("STRING"),
            L::Other {
                native: "STRING".into()
            }
        );
        assert_eq!(ty("VARCHAR2(10)"), L::Text { unicode: true });
    }

    #[test]
    fn round_trips_integer_sizes() {
        for b in [1u8, 2, 3, 4, 8] {
            let r = Sqlite.render_type(&L::int(b));
            assert_eq!(ty(&r.native), L::int(b));
        }
    }

    /// The native name and the worst note's severity (None: exact).
    fn render(t: L) -> (String, Option<Severity>) {
        let r = Sqlite.render_type(&t);
        (r.native, r.notes.iter().map(|n| n.severity).max())
    }

    #[test]
    fn renders_every_type_and_says_what_it_loses() {
        use Severity::*;
        let cases = [
            (L::Bool, "BOOLEAN", None),
            (
                L::Int {
                    bytes: 8,
                    unsigned: true,
                },
                "NUMERIC",
                Some(Loss),
            ),
            (
                L::Int {
                    bytes: 4,
                    unsigned: true,
                },
                "BIGINT",
                None,
            ),
            (
                L::Decimal {
                    precision: Some(10),
                    scale: Some(2),
                },
                "NUMERIC(10, 2)",
                Some(Loss),
            ),
            (
                L::Decimal {
                    precision: Some(9),
                    scale: None,
                },
                "NUMERIC(9)",
                None,
            ),
            (
                L::Decimal {
                    precision: None,
                    scale: None,
                },
                "NUMERIC",
                Some(Loss),
            ),
            (L::Float { bytes: 4 }, "REAL", None),
            (L::Money, "NUMERIC", Some(Loss)),
            (
                L::Char {
                    len: None,
                    unicode: false,
                },
                "CHAR",
                None,
            ),
            (
                L::Char {
                    len: Some(2),
                    unicode: false,
                },
                "CHAR(2)",
                None,
            ),
            (
                L::Varchar {
                    len: Some(9),
                    unicode: true,
                },
                "VARCHAR(9)",
                None,
            ),
            (
                L::Varchar {
                    len: None,
                    unicode: true,
                },
                "TEXT",
                None,
            ),
            (L::Varbinary { len: Some(4) }, "BLOB", None),
            (L::Bit { len: Some(8) }, "BLOB", Some(Warning)),
            (L::Date, "DATE", None),
            (
                L::Time {
                    precision: None,
                    tz: false,
                },
                "TIME",
                None,
            ),
            (
                L::Time {
                    precision: None,
                    tz: true,
                },
                "TIME",
                Some(Loss),
            ),
            (
                L::Timestamp {
                    precision: Some(3),
                    tz: true,
                },
                "DATETIME",
                Some(Loss),
            ),
            (L::Interval, "TEXT", Some(Warning)),
            (L::Year, "INTEGER", None),
            (L::Uuid, "TEXT", Some(Info)),
            (L::Json { binary: true }, "TEXT", Some(Info)),
            (L::Xml, "TEXT", None),
            (
                L::Set {
                    values: vec!["a".into(), "b".into()],
                },
                "TEXT",
                Some(Warning),
            ),
            (
                L::Map {
                    key: Box::new(L::Uuid),
                    value: Box::new(L::Bool),
                },
                "TEXT",
                Some(Warning),
            ),
            (
                L::Geometry {
                    kind: None,
                    srid: None,
                    geography: false,
                },
                "BLOB",
                Some(Warning),
            ),
            (L::MacAddr, "TEXT", None),
            (L::RowVersion, "BLOB", Some(Warning)),
            (
                L::Other {
                    native: "STRING".into(),
                },
                "STRING",
                None,
            ),
        ];
        for (t, native, worst) in cases {
            let got = render(t.clone());
            assert_eq!(got, (native.to_string(), worst), "{t:?}");
        }
        // An enum's values are listed in its note.
        let r = Sqlite.render_type(&L::Enum {
            values: vec!["rojo".into(), "verde".into()],
        });
        assert!(r.notes[0].message.contains("rojo, verde"), "{:?}", r.notes);
    }

    #[test]
    fn defaults_sqlite_can_compute() {
        let uuid_text = Sqlite
            .render_default(&DefaultValue::NewUuid, &L::Uuid)
            .unwrap();
        assert!(uuid_text.contains("randomblob"));
        assert_eq!(
            Sqlite.render_default(
                &DefaultValue::NewUuid,
                &L::Varchar {
                    len: Some(36),
                    unicode: false
                }
            ),
            Some(uuid_text)
        );
        assert_eq!(
            Sqlite
                .render_default(&DefaultValue::NewUuid, &L::Binary { len: Some(16) })
                .as_deref(),
            Some("(randomblob(16))")
        );
        // Too short to hold one: no default.
        assert_eq!(
            Sqlite.render_default(
                &DefaultValue::NewUuid,
                &L::Varchar {
                    len: Some(10),
                    unicode: false
                }
            ),
            None
        );
        assert_eq!(
            Sqlite
                .render_default(&DefaultValue::CurrentTimestamp, &L::Date)
                .as_deref(),
            Some("CURRENT_DATE")
        );
        assert_eq!(
            Sqlite
                .render_default(
                    &DefaultValue::CurrentTimestamp,
                    &L::Time {
                        precision: None,
                        tz: false
                    }
                )
                .as_deref(),
            Some("CURRENT_TIME")
        );
        assert_eq!(
            Sqlite
                .render_default(
                    &DefaultValue::CurrentTimestamp,
                    &L::Timestamp {
                        precision: None,
                        tz: false
                    }
                )
                .as_deref(),
            Some("CURRENT_TIMESTAMP")
        );
        let caps = Sqlite.caps();
        assert!(caps.partial_indexes && !caps.supports_include && !caps.comments);
    }

    fn table(cols: &[(&str, &str, bool)], pk: &[&str]) -> TableSchema {
        TableSchema {
            name: "t".into(),
            columns: cols
                .iter()
                .map(|(n, t, auto)| dbine_driver::ColumnDef {
                    name: n.to_string(),
                    data_type: t.to_string(),
                    auto_increment: *auto,
                    ..Default::default()
                })
                .collect(),
            primary_key: (!pk.is_empty()).then(|| dbine_driver::KeyDef {
                name: None,
                columns: pk.iter().map(|s| s.to_string()).collect(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn only_an_integer_key_of_one_column_auto_increments() {
        // A key of a whole-number type: INTEGER, quietly.
        let mut t = table(&[("id", "BIGINT", true)], &["id"]);
        let mut report = crate::issue::Report::default();
        Sqlite.finalize(&mut t, &mut report);
        assert_eq!(t.columns[0].data_type, "INTEGER");
        assert!(t.columns[0].auto_increment);
        assert!(report.issues.is_empty());
        // Unsigned or wider than 8 bytes: INTEGER, with the range noted.
        let mut t = table(&[("id", "NUMERIC(38)", true)], &["id"]);
        Sqlite.finalize(&mut t, &mut report);
        assert_eq!(t.columns[0].data_type, "INTEGER");
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].code, IssueCode::RangeLoss);
        // Not the key, a key of two columns, or not a whole number: dropped.
        for mut t in [
            table(&[("id", "INT", true), ("k", "INT", false)], &["k"]),
            table(&[("id", "INT", true), ("k", "INT", false)], &["id", "k"]),
            table(&[("id", "NUMERIC(10, 2)", true)], &["id"]),
        ] {
            let mut report = crate::issue::Report::default();
            Sqlite.finalize(&mut t, &mut report);
            assert!(!t.columns[0].auto_increment);
            assert_eq!(report.issues[0].code, IssueCode::AutoIncrementDropped);
        }
    }
}
