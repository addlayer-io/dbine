//! Cloud Spanner, GoogleSQL dialect (the one DBine's driver speaks; a
//! PostgreSQL-dialect database reports PostgreSQL names and isn't served by
//! the driver). Types: BOOL, INT64, FLOAT32/64, NUMERIC (fixed 38, 9),
//! STRING(n|MAX), BYTES(n|MAX), DATE, TIMESTAMP (an instant, to the
//! nanosecond), JSON, ARRAY<T>. Every table needs a primary key; identity
//! columns exist (INT64, bit-reversed, not consecutive); foreign keys take
//! only ON DELETE CASCADE / NO ACTION.
//!
//! Embeddings are `ARRAY<FLOAT32>` / `ARRAY<FLOAT64>` with
//! `(vector_length=>n)`, which a vector index needs; vector indexes take
//! COSINE, EUCLIDEAN or DOT_PRODUCT and the tree's sizes in `OPTIONS`.

use super::bigquery::{generic, nested};
use super::postgres::{longest, precision_loss};
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{ColumnDef, KeyDef, TableSchema};

pub struct Spanner;

const MAX_STRING: u32 = 2_621_440;
const MAX_BYTES: u32 = 10_485_760;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Spanner = Spanner;
    (driver_id == "spanner").then_some(&D as &dyn Dialect)
}

/// A GoogleSQL literal as the Spanner driver writes them: the quote as
/// `\x27`, so statement splitting (which knows no backslash escapes) holds.
fn literal(s: &str) -> String {
    let mut o = String::from("'");
    for ch in s.chars() {
        match ch {
            '\\' => o.push_str("\\\\"),
            '\'' => o.push_str("\\x27"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o.push('\'');
    o
}

/// `ARRAY<FLOAT32>(vector_length=>3)` → (`ARRAY<FLOAT32>`, 3).
pub(super) fn vector_length(raw: &str) -> Option<(&str, u32)> {
    let s = raw.trim();
    let open = s.rfind('(')?;
    let inner = s[open + 1..].strip_suffix(')')?;
    let (k, v) = inner.split_once("=>")?;
    if !k.trim().eq_ignore_ascii_case("vector_length") {
        return None;
    }
    let n = v.trim().parse().ok()?;
    Some((s[..open].trim(), n))
}

fn string(len: Option<u32>) -> String {
    match len {
        Some(n) if n <= MAX_STRING => format!("STRING({n})"),
        _ => "STRING(MAX)".into(),
    }
}

impl Dialect for Spanner {
    fn id(&self) -> &'static str {
        "spanner"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let len = || if t.is_max() { None } else { t.arg_u32(0) };
        if let Some((array, dims)) = vector_length(&t.raw) {
            return match generic(array).map(|(h, a)| (h, a.first().map(|e| nested(self, e)))) {
                Some((h, Some(L::Float { bytes }))) if h == "array" => L::Vector {
                    dims: Some(dims),
                    element: if bytes == 4 {
                        dbine_driver::VectorElement::F32
                    } else {
                        dbine_driver::VectorElement::F64
                    },
                    sparse: false,
                },
                _ => L::Other {
                    native: t.raw.clone(),
                },
            };
        }
        if let Some((head, args)) = generic(&t.raw) {
            return match head.as_str() {
                "array" if args.len() == 1 => L::Array {
                    of: Box::new(nested(self, &args[0])),
                },
                _ => L::Other {
                    native: t.raw.clone(),
                },
            };
        }
        match t.name.as_str() {
            "bool" | "boolean" => L::Bool,
            "int64" => L::int(8),
            "float32" => L::Float { bytes: 4 },
            "float64" => L::Float { bytes: 8 },
            "numeric" => L::Decimal {
                precision: Some(38),
                scale: Some(9),
            },
            "string" => match len() {
                Some(n) => L::Varchar {
                    len: Some(n),
                    unicode: true,
                },
                None => L::Text { unicode: true },
            },
            "bytes" => match len() {
                Some(n) => L::Varbinary { len: Some(n) },
                None => L::Blob,
            },
            "date" => L::Date,
            "timestamp" => L::Timestamp {
                precision: Some(9),
                tz: true,
            },
            "json" => L::Json { binary: true },
            "uuid" => L::Uuid,
            "interval" => L::Interval,
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
                // FLOAT32 holds halves, bfloat16, bytes and bits exactly;
                // FLOAT64 32-bit integers.
                let (of, r) = match element {
                    E::F64 | E::I32 => ("FLOAT64", None),
                    E::Other => (
                        "FLOAT64",
                        Some("el tipo de sus números no se conoce: quedan como FLOAT64"),
                    ),
                    _ => ("FLOAT32", None),
                };
                let r0 = match dims {
                    Some(n) => Rendered::exact(format!("ARRAY<{of}>(vector_length=>{n})")),
                    None => Rendered::exact(format!("ARRAY<{of}>")).with(
                        Info,
                        TypeChanged,
                        "El vector no declara su dimensión: queda sin vector_length, y Spanner no le crea un índice vectorial hasta fijarla.",
                    ),
                };
                match r {
                    Some(m) => r0.with(Info, TypeChanged, format!("{}: {m}.", t.describe())),
                    None => r0,
                }
            }
            L::Bool => Rendered::exact("BOOL"),
            L::Int { bytes, unsigned } => match L::signed_bytes_for(*bytes, *unsigned) {
                1..=8 => Rendered::exact("INT64"),
                _ if *bytes == 8 => Rendered::exact("NUMERIC").with(
                    Info,
                    TypeChanged,
                    "Entero de 8 bytes sin signo como NUMERIC.",
                ),
                _ => Rendered::exact("NUMERIC").with(
                    Loss,
                    RangeLoss,
                    "Entero de 16 bytes como NUMERIC: guarda hasta 29 dígitos enteros.",
                ),
            },
            L::Decimal { precision, scale } => {
                let (p, s) = (precision.unwrap_or(0), scale.unwrap_or(0));
                let r = Rendered::exact("NUMERIC");
                match precision {
                    None => r.with(Loss, PrecisionLoss, "NUMERIC de Spanner guarda hasta 29 dígitos enteros y 9 decimales; el origen no fija la precisión."),
                    Some(_) if s > 9 || p.saturating_sub(s) > 29 => {
                        r.with(Loss, PrecisionLoss, format!("NUMERIC de Spanner guarda hasta 29 dígitos enteros y 9 decimales; el origen es ({p}, {s})."))
                    }
                    Some(_) => r,
                }
            }
            L::Float { bytes: 4 } => Rendered::exact("FLOAT32"),
            L::Float { .. } => Rendered::exact("FLOAT64"),
            L::Money => Rendered::exact("NUMERIC").with(Info, TypeChanged, "Moneda como NUMERIC."),
            L::Char { len, .. } => Rendered::exact(string(*len)).with(
                Info,
                TypeChanged,
                "Spanner no tiene texto de largo fijo: no se rellena con espacios.",
            ),
            L::Varchar { len: Some(n), .. } if *n <= MAX_STRING => {
                Rendered::exact(format!("STRING({n})"))
            }
            L::Varchar { .. } | L::Text { .. } => Rendered::exact("STRING(MAX)"),
            L::Binary { len: Some(n) } | L::Varbinary { len: Some(n) } if *n <= MAX_BYTES => {
                Rendered::exact(format!("BYTES({n})"))
            }
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("BYTES(MAX)"),
            L::Bit { len } => Rendered::exact(len.map_or("BYTES(MAX)".into(), |n| {
                format!("BYTES({})", n.div_ceil(8).max(1))
            }))
            .with(
                Warning,
                TypeApproximated,
                "Spanner no tiene cadenas de bits: se guarda como binario.",
            ),
            L::Date => Rendered::exact("DATE"),
            L::Time { tz, .. } => {
                let r = Rendered::exact("STRING(32)").with(
                    Warning,
                    TypeApproximated,
                    "Spanner no tiene tipo hora: queda como texto HH:MM:SS.",
                );
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Se pierde la zona horaria de la hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact("TIMESTAMP").with_loss(precision_loss(*precision, 9));
                if *tz {
                    r
                } else {
                    r.with(Info, TimeZoneLoss, "TIMESTAMP de Spanner es un instante: los valores sin zona hay que cargarlos con zona explícita (si no, Spanner supone America/Los_Angeles).")
                }
            }
            L::Interval => Rendered::exact("STRING(64)").with(
                Warning,
                TypeApproximated,
                "Spanner no guarda intervalos en columnas: queda como texto.",
            ),
            L::Year => Rendered::exact("INT64").with(Info, TypeChanged, "Año como INT64."),
            L::Uuid => {
                Rendered::exact("STRING(36)").with(Info, TypeChanged, "UUID como STRING(36).")
            }
            L::Json { .. } => Rendered::exact("JSON"),
            L::Xml => {
                Rendered::exact("STRING(MAX)").with(Info, TypeApproximated, "XML como texto.")
            }
            L::Enum { values } => Rendered::exact(format!("STRING({})", longest(values))).with(
                Warning,
                TypeApproximated,
                format!(
                    "Spanner no tiene enumerados: queda como texto. Valores: {}.",
                    values.join(", ")
                ),
            ),
            L::Set { values } => Rendered::exact(format!("ARRAY<STRING({})>", longest(values)))
                .with(
                    Warning,
                    TypeApproximated,
                    format!(
                        "Conjunto como arreglo de texto. Valores: {}.",
                        values.join(", ")
                    ),
                ),
            L::Array { of } if matches!(**of, L::Array { .. }) => Rendered::exact("JSON").with(
                Warning,
                TypeApproximated,
                "Spanner no tiene arreglos de arreglos: se guarda como JSON.",
            ),
            L::Array { of } => {
                let inner = self.render_type(of);
                Rendered {
                    native: format!("ARRAY<{}>", inner.native),
                    notes: inner.notes,
                }
            }
            L::Map { .. } => Rendered::exact("JSON").with(
                Warning,
                TypeApproximated,
                "Spanner no tiene mapas: se guarda como JSON.",
            ),
            L::Geometry { .. } => Rendered::exact("STRING(MAX)").with(
                Warning,
                TypeApproximated,
                "Spanner no tiene tipos espaciales: queda como texto (WKT).",
            ),
            L::Inet => Rendered::exact("STRING(45)").with(
                Info,
                TypeApproximated,
                "Dirección IP como texto.",
            ),
            L::MacAddr => Rendered::exact("STRING(17)").with(
                Info,
                TypeApproximated,
                "Dirección MAC como texto.",
            ),
            L::RowVersion => Rendered::exact("BYTES(8)").with(
                Warning,
                TypeApproximated,
                "Spanner no tiene versión de fila automática.",
            ),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::Null => "NULL".into(),
            // A FLOAT64 literal doesn't coerce to NUMERIC (an INT64 one does).
            DefaultValue::Number(n)
                if matches!(ty, L::Decimal { .. } | L::Money) && n.parse::<i64>().is_err() =>
            {
                format!("NUMERIC '{n}'")
            }
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => literal(s),
            DefaultValue::Bool(b) if matches!(ty, L::Bool) => {
                if *b { "TRUE" } else { "FALSE" }.into()
            }
            DefaultValue::Bool(b) => if *b { "1" } else { "0" }.into(),
            DefaultValue::CurrentTimestamp if matches!(ty, L::Date) => "CURRENT_DATE()".into(),
            DefaultValue::CurrentTimestamp => "CURRENT_TIMESTAMP()".into(),
            DefaultValue::CurrentDate => "CURRENT_DATE()".into(),
            DefaultValue::CurrentTime => "FORMAT_TIMESTAMP('%H:%M:%S', CURRENT_TIMESTAMP())".into(),
            DefaultValue::NewUuid => "GENERATE_UUID()".into(),
            DefaultValue::NextVal(_) | DefaultValue::Expr(_) => return None,
        })
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            on_delete: &["CASCADE", "NO ACTION"],
            on_update: &[],
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: false,
            max_identifier: 128,
            case: IdentCase::Preserve,
            vector_indexes: true,
            vectors: true,
        }
    }

    /// COSINE, EUCLIDEAN or DOT_PRODUCT (the distance is required). An
    /// IVF index's lists are the tree's leaves (`num_leaves`); HNSW's
    /// settings have no counterpart. Spanner indexes only a dense vector
    /// column with `vector_length`, so a searched vector without a known
    /// dimension (`convert` gives a graph list the one its index states)
    /// keeps its column but not the index.
    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
        ctx: &super::VectorIndexCtx<'_>,
    ) -> Result<super::VectorIndexMapping, String> {
        let column = ctx.column;
        use dbine_driver::vector::{metric, param};
        match column {
            Some(L::Vector { sparse: true, .. }) => {
                return Err("Spanner no tiene vectores dispersos".into())
            }
            Some(L::Vector { dims: None, .. }) => return Err(
                "Spanner lo hace solo sobre un ARRAY<FLOAT32> o ARRAY<FLOAT64> con vector_length, y el vector de origen no declara su dimensión; fijala y crealo a mano"
                    .into(),
            ),
            _ => {}
        }
        let metric = match v.metric.as_deref() {
            Some(m @ (metric::COSINE | metric::L2 | metric::IP)) => m.to_string(),
            Some(m) => {
                return Err(format!(
                    "Spanner no tiene la distancia «{m}» (solo coseno, euclídea y producto escalar)"
                ))
            }
            None => return Err(
                "Spanner necesita la distancia del índice (distance_type) y el origen no la dice"
                    .into(),
            ),
        };
        let params = v
            .params
            .iter()
            .filter(|(k, _)| k.as_str() == param::LISTS)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(super::VectorIndexMapping::new(
            Some("VECTOR".into()),
            dbine_driver::VectorIndex {
                algorithm: None,
                metric: Some(metric),
                params,
            },
        ))
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        use IssueCode::*;
        use Severity::*;
        for c in t.columns.iter_mut().filter(|c| c.auto_increment) {
            if c.data_type != "INT64" {
                report.push(
                    Loss,
                    RangeLoss,
                    &t.name,
                    Some(&c.name),
                    format!(
                        "Las columnas de identidad de Spanner son INT64: {} pasa a INT64.",
                        c.data_type
                    ),
                );
                c.data_type = "INT64".into();
            }
            report.push(
                Info,
                AutoIncrementChanged,
                &t.name,
                Some(&c.name),
                "Identidad con secuencia bit-reversed: los valores son únicos pero no consecutivos.",
            );
        }
        // Every table needs a primary key: a generated UUID column.
        if t.primary_key.as_ref().is_none_or(|k| k.columns.is_empty()) {
            let mut name = "row_id".to_string();
            let mut i = 1;
            while t.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&name)) {
                name = format!("row_id_{i}");
                i += 1;
            }
            t.columns.insert(
                0,
                ColumnDef {
                    name: name.clone(),
                    data_type: "STRING(36)".into(),
                    nullable: false,
                    default_value: Some("GENERATE_UUID()".into()),
                    ..Default::default()
                },
            );
            t.primary_key = Some(KeyDef {
                name: None,
                columns: vec![name.clone()],
            });
            report.push(
                Warning,
                PrimaryKeyAdded,
                &t.name,
                Some(&name),
                format!("Spanner exige clave primaria: se agrega «{name}» con un UUID generado."),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::logical_of;
    use crate::dialect::VectorIndexPair as _;
    use crate::parse::parse;

    fn lt(s: &str) -> L {
        logical_of(&Spanner, &parse(s))
    }

    #[test]
    fn vector_length_arrays_are_vectors() {
        use dbine_driver::VectorElement as E;
        let v = |dims, element| L::Vector {
            dims,
            element,
            sparse: false,
        };
        assert_eq!(lt("ARRAY<FLOAT32>(vector_length=>3)"), v(Some(3), E::F32));
        assert_eq!(
            lt("ARRAY<FLOAT64>( vector_length => 768 )"),
            v(Some(768), E::F64)
        );
        assert!(matches!(
            lt("ARRAY<STRING(MAX)>(vector_length=>3)"),
            L::Other { .. }
        ));
        // Without vector_length an array of floats is an array.
        assert_eq!(
            lt("ARRAY<FLOAT32>"),
            L::Array {
                of: Box::new(L::Float { bytes: 4 })
            }
        );
        let r = |t: L| Spanner.render_type(&t);
        assert_eq!(
            r(v(Some(3), E::F32)).native,
            "ARRAY<FLOAT32>(vector_length=>3)"
        );
        assert!(r(v(Some(3), E::F32)).notes.is_empty());
        assert_eq!(
            r(v(Some(8), E::F64)).native,
            "ARRAY<FLOAT64>(vector_length=>8)"
        );
        assert_eq!(
            r(v(Some(8), E::F16)).native,
            "ARRAY<FLOAT32>(vector_length=>8)"
        );
        assert_eq!(
            r(v(Some(8), E::I8)).native,
            "ARRAY<FLOAT32>(vector_length=>8)"
        );
        assert_eq!(
            r(v(Some(8), E::Other)).native,
            "ARRAY<FLOAT64>(vector_length=>8)"
        );
        let none = r(v(None, E::F32));
        assert_eq!(none.native, "ARRAY<FLOAT32>");
        assert_eq!(none.notes[0].code, IssueCode::TypeChanged);
        assert_eq!(
            r(L::Vector {
                dims: Some(4),
                element: E::F32,
                sparse: true
            })
            .native,
            "JSON"
        );
        // Round trip.
        for s in [
            "ARRAY<FLOAT32>(vector_length=>3)",
            "ARRAY<FLOAT64>(vector_length=>1536)",
        ] {
            assert_eq!(r(lt(s)).native, s);
        }
    }

    /// A Spanner embedding table to BigQuery, Snowflake and Neo4j, and a
    /// vector index each way: the dimension and distance carry over.
    #[test]
    fn vector_tables_cross_families() {
        use dbine_driver::{ColumnDef, IndexDef, VectorIndex};
        let mut vi = VectorIndex {
            algorithm: None,
            metric: Some("cosine".into()),
            ..Default::default()
        };
        vi.params.insert("lists".into(), "100".into());
        vi.params.insert("tree_depth".into(), "2".into());
        let t = TableSchema {
            name: "docs".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: "INT64".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnDef {
                    name: "emb".into(),
                    data_type: "ARRAY<FLOAT32>(vector_length=>3)".into(),
                    nullable: true,
                    ..Default::default()
                },
            ],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into()],
            }),
            indexes: vec![IndexDef {
                name: "docs_emb".into(),
                columns: vec!["emb".into()],
                kind: Some("VECTOR".into()),
                vector: Some(vi),
                ..Default::default()
            }],
            ..Default::default()
        };
        let conv = |to: &str| {
            crate::convert(std::slice::from_ref(&t), "spanner", to, &Default::default()).unwrap()
        };
        let col = |c: &crate::Conversion, n: &str| {
            c.tables[0]
                .columns
                .iter()
                .find(|x| x.name.eq_ignore_ascii_case(n))
                .unwrap()
                .data_type
                .clone()
        };

        let bq = conv("bigquery");
        assert_eq!(col(&bq, "emb"), "ARRAY<FLOAT64>");
        let ix = &bq.tables[0].indexes[0];
        assert_eq!(ix.kind.as_deref(), Some("VECTOR"));
        let v = ix.vector.as_ref().unwrap();
        assert_eq!(v.algorithm.as_deref(), Some("ivf"));
        assert_eq!(v.metric.as_deref(), Some("cosine"));
        assert_eq!(v.params.get("lists").map(String::as_str), Some("100"));
        assert!(!v.params.contains_key("tree_depth"));

        let sf = conv("snowflake");
        assert_eq!(col(&sf, "EMB"), "VECTOR(FLOAT, 3)");
        // Snowflake has no vector indexes: said, not silently dropped.
        assert!(sf.tables[0].indexes.iter().all(|i| i.vector.is_none()));
        assert!(sf.issues.iter().any(|i| i.code == IssueCode::IndexDropped));

        let neo = conv("neo4j");
        assert_eq!(col(&neo, "emb"), "LIST<FLOAT>");
        let ix = neo.tables[0]
            .indexes
            .iter()
            .find(|i| i.kind.as_deref() == Some("VECTOR"))
            .unwrap();
        assert_eq!(
            ix.vector.as_ref().unwrap().metric.as_deref(),
            Some("cosine")
        );

        // And back: BigQuery's ARRAY<FLOAT64> has no dimension, so Spanner
        // gets no vector_length, and its vector index (which needs one) is
        // left out and said, not written to fail.
        let back = crate::convert(&bq.tables, "bigquery", "spanner", &Default::default()).unwrap();
        assert_eq!(col(&back, "emb"), "ARRAY<FLOAT64>");
        assert!(back.tables[0].indexes.iter().all(|i| i.vector.is_none()));
        assert!(back
            .issues
            .iter()
            .any(|i| i.code == IssueCode::IndexDropped && i.object.as_deref() == Some("docs_emb")));
        let back = crate::convert(&sf.tables, "snowflake", "spanner", &Default::default()).unwrap();
        assert_eq!(col(&back, "EMB"), "ARRAY<FLOAT32>(vector_length=>3)");
    }

    /// A graph label's float list with a vector index that states its
    /// dimension: the column takes it as vector_length and keeps the index.
    #[test]
    fn graph_vector_indexes_give_the_dimension() {
        use dbine_driver::{ColumnDef, IndexDef, VectorIndex};
        let index = |dims: Option<&str>| {
            let mut v = VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some("cosine".into()),
                ..Default::default()
            };
            if let Some(d) = dims {
                v.params.insert("dimensions".into(), d.into());
            }
            v.params.insert("m".into(), "16".into());
            IndexDef {
                name: "doc_emb".into(),
                columns: vec!["emb".into()],
                kind: Some("VECTOR".into()),
                vector: Some(v),
                ..Default::default()
            }
        };
        let label = |ix: IndexDef| TableSchema {
            kind: "label".into(),
            name: "Doc".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: "INTEGER".into(),
                    nullable: true,
                    ..Default::default()
                },
                ColumnDef {
                    name: "emb".into(),
                    data_type: "LIST<FLOAT>".into(),
                    nullable: true,
                    ..Default::default()
                },
            ],
            indexes: vec![ix],
            ..Default::default()
        };
        for from in ["neo4j", "memgraph"] {
            let c = crate::convert(
                &[label(index(Some("3")))],
                from,
                "spanner",
                &Default::default(),
            )
            .unwrap();
            let t = &c.tables[0];
            let emb = t.columns.iter().find(|c| c.name == "emb").unwrap();
            assert_eq!(emb.data_type, "ARRAY<FLOAT64>(vector_length=>3)", "{from}");
            let ix = t.indexes.iter().find(|i| i.vector.is_some()).unwrap();
            let v = ix.vector.as_ref().unwrap();
            assert_eq!(v.metric.as_deref(), Some("cosine"));
            assert!(!v.params.contains_key("dimensions"));
            assert_eq!(
                c.columns
                    .iter()
                    .find(|m| m.column == "emb")
                    .unwrap()
                    .target_type,
                emb.data_type
            );

            // No dimension anywhere: the index is left out, and said.
            let c = crate::convert(&[label(index(None))], from, "spanner", &Default::default())
                .unwrap();
            assert!(c.tables[0].indexes.iter().all(|i| i.vector.is_none()));
            assert!(c.issues.iter().any(
                |i| i.code == IssueCode::IndexDropped && i.object.as_deref() == Some("doc_emb")
            ));
        }
    }

    #[test]
    fn vector_indexes_in_spanner_terms() {
        use dbine_driver::VectorIndex as V;
        let v = |alg: Option<&str>, m: Option<&str>, p: &[(&str, &str)]| V {
            algorithm: alg.map(str::to_string),
            metric: m.map(str::to_string),
            params: p
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let (kind, got) = Spanner
            .vector_index_pair(&v(Some("ivfflat"), Some("l2"), &[("lists", "100")]), None)
            .unwrap();
        assert_eq!(kind.as_deref(), Some("VECTOR"));
        assert_eq!(got, v(None, Some("l2"), &[("lists", "100")]));
        let (_, got) = Spanner
            .vector_index_pair(&v(Some("hnsw"), Some("cosine"), &[("m", "16")]), None)
            .unwrap();
        assert_eq!(got, v(None, Some("cosine"), &[]));
        assert!(Spanner
            .vector_index_pair(&v(None, Some("l1"), &[]), None)
            .is_err());
        assert!(Spanner
            .vector_index_pair(&v(None, None, &[]), None)
            .is_err());
        // The searched column: a vector of known dimension takes the index;
        // one without a dimension, or a sparse one, doesn't.
        let col = |dims, sparse| L::Vector {
            dims,
            element: dbine_driver::VectorElement::F32,
            sparse,
        };
        let ok = v(None, Some("cosine"), &[("dimensions", "3")]);
        let (_, got) = Spanner
            .vector_index_pair(&ok, Some(&col(Some(3), false)))
            .unwrap();
        assert_eq!(got, v(None, Some("cosine"), &[]));
        assert!(Spanner
            .vector_index_pair(&ok, Some(&col(None, false)))
            .unwrap_err()
            .contains("vector_length"));
        assert!(Spanner
            .vector_index_pair(&ok, Some(&col(Some(3), true)))
            .unwrap_err()
            .contains("dispersos"));
    }

    #[test]
    fn parses_catalog_spellings() {
        assert_eq!(lt("INT64"), L::int(8));
        assert_eq!(lt("FLOAT32"), L::Float { bytes: 4 });
        assert_eq!(lt("FLOAT64"), L::Float { bytes: 8 });
        assert_eq!(
            lt("NUMERIC"),
            L::Decimal {
                precision: Some(38),
                scale: Some(9)
            }
        );
        assert_eq!(
            lt("STRING(20)"),
            L::Varchar {
                len: Some(20),
                unicode: true
            }
        );
        assert_eq!(lt("STRING(MAX)"), L::Text { unicode: true });
        assert_eq!(lt("BYTES(10)"), L::Varbinary { len: Some(10) });
        assert_eq!(lt("BYTES(MAX)"), L::Blob);
        assert_eq!(
            lt("TIMESTAMP"),
            L::Timestamp {
                precision: Some(9),
                tz: true
            }
        );
        assert_eq!(lt("DATE"), L::Date);
        assert_eq!(lt("JSON"), L::Json { binary: true });
        assert_eq!(lt("BOOL"), L::Bool);
        assert_eq!(
            lt("ARRAY<STRING(5)>"),
            L::Array {
                of: Box::new(L::Varchar {
                    len: Some(5),
                    unicode: true
                })
            }
        );
        assert!(matches!(lt("PROTO<a.b.C>"), L::Other { .. }));
        assert!(matches!(lt("TOKENLIST"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| Spanner.render_type(&t);
        assert_eq!(r(L::Bool).native, "BOOL");
        assert_eq!(
            r(L::Int {
                bytes: 4,
                unsigned: true
            })
            .native,
            "INT64"
        );
        assert_eq!(
            r(L::Int {
                bytes: 8,
                unsigned: true
            })
            .native,
            "NUMERIC"
        );
        assert_eq!(r(L::int(16)).notes[0].code, IssueCode::RangeLoss);
        assert!(r(L::Decimal {
            precision: Some(12),
            scale: Some(2)
        })
        .notes
        .is_empty());
        assert_eq!(
            r(L::Decimal {
                precision: Some(38),
                scale: Some(0)
            })
            .notes[0]
                .code,
            IssueCode::PrecisionLoss
        );
        assert_eq!(
            r(L::Decimal {
                precision: None,
                scale: None
            })
            .notes[0]
                .code,
            IssueCode::PrecisionLoss
        );
        assert_eq!(r(L::Float { bytes: 4 }).native, "FLOAT32");
        assert_eq!(r(L::Float { bytes: 8 }).native, "FLOAT64");
        assert_eq!(r(L::Money).native, "NUMERIC");
        assert_eq!(
            r(L::Char {
                len: Some(3),
                unicode: true
            })
            .native,
            "STRING(3)"
        );
        assert_eq!(
            r(L::Varchar {
                len: Some(30),
                unicode: true
            })
            .native,
            "STRING(30)"
        );
        assert_eq!(
            r(L::Varchar {
                len: Some(10_000_000),
                unicode: true
            })
            .native,
            "STRING(MAX)"
        );
        assert_eq!(r(L::Text { unicode: false }).native, "STRING(MAX)");
        assert_eq!(r(L::Binary { len: Some(16) }).native, "BYTES(16)");
        assert_eq!(r(L::Varbinary { len: None }).native, "BYTES(MAX)");
        assert_eq!(r(L::Blob).native, "BYTES(MAX)");
        assert_eq!(r(L::Bit { len: Some(9) }).native, "BYTES(2)");
        assert_eq!(r(L::Date).native, "DATE");
        assert_eq!(
            r(L::Time {
                precision: None,
                tz: true
            })
            .native,
            "STRING(32)"
        );
        assert_eq!(
            r(L::Timestamp {
                precision: Some(6),
                tz: true
            })
            .native,
            "TIMESTAMP"
        );
        assert_eq!(
            r(L::Timestamp {
                precision: Some(12),
                tz: true
            })
            .notes[0]
                .code,
            IssueCode::PrecisionLoss
        );
        assert_eq!(r(L::Interval).native, "STRING(64)");
        assert_eq!(r(L::Year).native, "INT64");
        assert_eq!(r(L::Uuid).native, "STRING(36)");
        assert_eq!(r(L::Json { binary: false }).native, "JSON");
        assert_eq!(r(L::Xml).native, "STRING(MAX)");
        assert_eq!(
            r(L::Enum {
                values: vec!["abc".into()]
            })
            .native,
            "STRING(3)"
        );
        assert_eq!(
            r(L::Set {
                values: vec!["ab".into()]
            })
            .native,
            "ARRAY<STRING(2)>"
        );
        assert_eq!(
            r(L::Array {
                of: Box::new(L::Text { unicode: true })
            })
            .native,
            "ARRAY<STRING(MAX)>"
        );
        assert_eq!(
            r(L::Array {
                of: Box::new(L::Array {
                    of: Box::new(L::Bool)
                })
            })
            .native,
            "JSON"
        );
        assert_eq!(
            r(L::Map {
                key: Box::new(L::Bool),
                value: Box::new(L::Bool)
            })
            .native,
            "JSON"
        );
        assert_eq!(
            r(L::Geometry {
                kind: None,
                srid: None,
                geography: true
            })
            .native,
            "STRING(MAX)"
        );
        assert_eq!(r(L::Inet).native, "STRING(45)");
        assert_eq!(r(L::MacAddr).native, "STRING(17)");
        assert_eq!(r(L::RowVersion).native, "BYTES(8)");
    }

    #[test]
    fn defaults() {
        let d = Spanner;
        assert_eq!(
            d.render_default(
                &DefaultValue::CurrentTimestamp,
                &L::Timestamp {
                    precision: None,
                    tz: true
                }
            )
            .as_deref(),
            Some("CURRENT_TIMESTAMP()")
        );
        assert_eq!(
            d.render_default(
                &DefaultValue::Text("it's".into()),
                &L::Text { unicode: true }
            )
            .as_deref(),
            Some("'it\\x27s'")
        );
        assert_eq!(
            d.render_default(
                &DefaultValue::Number("1.5".into()),
                &L::Decimal {
                    precision: Some(5),
                    scale: Some(2)
                }
            )
            .as_deref(),
            Some("NUMERIC '1.5'")
        );
        assert_eq!(
            d.render_default(
                &DefaultValue::Number("0".into()),
                &L::Decimal {
                    precision: Some(5),
                    scale: Some(2)
                }
            )
            .as_deref(),
            Some("0")
        );
        assert_eq!(
            d.render_default(&DefaultValue::NewUuid, &L::Uuid)
                .as_deref(),
            Some("GENERATE_UUID()")
        );
    }

    #[test]
    fn finalize_adds_a_key_and_fixes_identity() {
        let mut t = TableSchema {
            name: "t".into(),
            columns: vec![
                ColumnDef {
                    name: "row_id".into(),
                    data_type: "INT64".into(),
                    ..Default::default()
                },
                ColumnDef {
                    name: "n".into(),
                    data_type: "NUMERIC".into(),
                    auto_increment: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut rep = Report::default();
        Spanner.finalize(&mut t, &mut rep);
        assert_eq!(
            t.primary_key.as_ref().unwrap().columns,
            vec!["row_id_1".to_string()]
        );
        assert_eq!(
            t.columns[0].default_value.as_deref(),
            Some("GENERATE_UUID()")
        );
        assert_eq!(t.columns[2].data_type, "INT64");
        assert!(rep
            .issues
            .iter()
            .any(|i| i.code == IssueCode::PrimaryKeyAdded));
    }
}
