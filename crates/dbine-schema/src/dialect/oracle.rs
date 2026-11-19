//! Oracle Database (and Autonomous Database).
//!
//! Types arrive as the driver builds them from `ALL_TAB_COLS`: `NUMBER`,
//! `NUMBER(10)`, `NUMBER(12,2)`, `INTEGER` (precision NULL, scale 0),
//! `FLOAT(126)`, `VARCHAR2(n)` / `CHAR(n)` with the length in characters,
//! `RAW(16)`, and the dictionary's own `TIMESTAMP(6) WITH TIME ZONE`,
//! `INTERVAL DAY(2) TO SECOND(6)`. Virtual columns carry
//! `… GENERATED ALWAYS AS (expr) VIRTUAL`.
//!
//! Character data is read and written as unicode: the database character
//! set is AL32UTF8 by default since 12.2, and lengths use CHAR semantics.

use super::postgres::{
    fit_decimal, longest, prec, precision_loss, set_len, single_auto_increment, uuid_slot, UuidSlot,
};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{parse, TypeSpec};
use dbine_driver::TableSchema;

pub struct Oracle;

/// Most dimensions of a `VECTOR`.
const MAX_VECTOR_DIMS: u32 = 65535;

/// `VECTOR(n, FLOAT32)`, `VECTOR(*, INT8)`, `VECTOR(n, FLOAT32, SPARSE)`…
fn render_vector(t: &L) -> Rendered {
    use dbine_driver::VectorElement as E;
    use IssueCode::*;
    use Severity::*;
    let L::Vector {
        dims,
        element,
        sparse,
    } = t
    else {
        return Rendered::exact("VECTOR(*, *)");
    };
    let mut notes: Vec<(Severity, IssueCode, String)> = Vec::new();
    let mut format = match element {
        E::F32 => "FLOAT32",
        E::F64 => "FLOAT64",
        E::I8 => "INT8",
        E::Bit => "BINARY",
        E::F16 | E::Bf16 => {
            notes.push((
                Info,
                TypeChanged,
                format!("{}: como FLOAT32 (sin pérdida).", t.describe()),
            ));
            "FLOAT32"
        }
        E::U8 | E::Other => {
            notes.push((
                Info,
                TypeChanged,
                format!("{}: como FLOAT32.", t.describe()),
            ));
            "FLOAT32"
        }
    };
    let dims = match dims {
        Some(n) if *n > MAX_VECTOR_DIMS => {
            notes.push((
                Warning,
                TypeApproximated,
                format!("Oracle admite vectores de hasta {MAX_VECTOR_DIMS} dimensiones: queda sin dimensión fija (*)."),
            ));
            "*".to_string()
        }
        Some(n) => {
            // BINARY packs 8 dimensions per byte.
            if format == "BINARY" && n % 8 != 0 {
                notes.push((
                    Info,
                    TypeChanged,
                    "Un vector BINARY de Oracle tiene una cantidad de dimensiones múltiplo de 8: queda como INT8.".to_string(),
                ));
                format = "INT8";
            }
            n.to_string()
        }
        None => "*".to_string(),
    };
    let native = if *sparse {
        if format == "BINARY" {
            notes.push((
                Info,
                TypeChanged,
                "Oracle no tiene vectores BINARY dispersos: queda FLOAT32.".to_string(),
            ));
            format = "FLOAT32";
        }
        format!("VECTOR({dims}, {format}, SPARSE)")
    } else {
        format!("VECTOR({dims}, {format})")
    };
    notes
        .into_iter()
        .fold(Rendered::exact(native), |r, (s, c, m)| r.with(s, c, m))
}

/// Longest VARCHAR2 with the default `MAX_STRING_SIZE = STANDARD`, in bytes.
const MAX_VARCHAR2: u32 = 4000;
/// Longest CHAR, in bytes.
const MAX_CHAR: u32 = 2000;
/// Bytes per character in AL32UTF8 (worst case).
const BYTES_PER_CHAR: u32 = 4;
/// Index key limit for an 8 KB block (ORA-01450 at 6398), with some room.
const MAX_KEY: u32 = 6000;

impl Dialect for Oracle {
    fn id(&self) -> &'static str {
        "oracle"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        // `VARCHAR2(20 CHAR)` / `(20 BYTE)`: the number comes first.
        let len = || {
            t.args
                .first()
                .and_then(|a| a.split_whitespace().next())
                .and_then(|n| n.parse::<u32>().ok())
        };
        match t.name.as_str() {
            "number" | "numeric" | "decimal" | "dec" => {
                let precision = t
                    .args
                    .first()
                    .filter(|a| a.trim() != "*")
                    .and_then(|a| a.trim().parse::<u32>().ok())
                    .or(t.args.first().filter(|a| a.trim() == "*").map(|_| 38));
                // A negative scale (NUMBER(10,-2)) rounds to tens, hundreds…: whole numbers.
                let scale = t
                    .args
                    .get(1)
                    .and_then(|a| a.trim().parse::<i32>().ok())
                    .map(|s| s.max(0) as u32);
                match (precision, scale) {
                    // NUMBER(p) / NUMBER(p, 0): a whole number of p digits.
                    (Some(pr), None | Some(0)) if pr <= 2 => L::int(1),
                    (Some(pr), None | Some(0)) if pr <= 4 => L::int(2),
                    (Some(pr), None | Some(0)) if pr <= 9 => L::int(4),
                    (Some(pr), None | Some(0)) if pr <= 18 => L::int(8),
                    (Some(pr), s) => L::Decimal {
                        precision: Some(pr),
                        scale: s.or(Some(0)),
                    },
                    (None, Some(0)) if !t.args.is_empty() => L::Decimal {
                        precision: Some(38),
                        scale: Some(0),
                    },
                    (None, _) => L::Decimal {
                        precision: None,
                        scale: None,
                    },
                }
            }
            // INTEGER / INT / SMALLINT are NUMBER(38).
            "integer" | "int" | "smallint" => L::Decimal {
                precision: Some(38),
                scale: Some(0),
            },
            "binary_float" => L::Float { bytes: 4 },
            "binary_double" => L::Float { bytes: 8 },
            // FLOAT(b) is a *decimal* NUMBER with b binary digits (126 by
            // default, ~38 decimal digits): binary floats only when b fits one.
            "float" | "real" | "double precision" => match p(0) {
                Some(b) if b <= 24 => L::Float { bytes: 4 },
                Some(b) if b <= 53 => L::Float { bytes: 8 },
                _ => L::Decimal {
                    precision: None,
                    scale: None,
                },
            },
            "char" | "character" | "nchar" => L::Char {
                len: len().or(Some(1)),
                unicode: true,
            },
            "varchar2" | "varchar" | "nvarchar2" => L::Varchar {
                len: len(),
                unicode: true,
            },
            "clob" | "long" | "nclob" => L::Text { unicode: true },
            "raw" => L::Varbinary { len: p(0) },
            "long raw" | "blob" => L::Blob,
            // Oracle's DATE carries a time of day, to the second.
            "date" => L::Timestamp {
                precision: Some(0),
                tz: false,
            },
            "timestamp" => L::Timestamp {
                precision: Some(p(0).unwrap_or(6).min(9) as u8),
                tz: t.with_tz,
            },
            "boolean" => L::Bool,
            "json" => L::Json { binary: true },
            // 23ai: `VECTOR(dims|*, FLOAT32|FLOAT64|INT8|BINARY|*, DENSE|SPARSE)`.
            "vector" => {
                use dbine_driver::VectorElement as E;
                let arg = |i: usize| t.args.get(i).map(|a| a.trim().to_ascii_uppercase());
                L::Vector {
                    dims: p(0),
                    element: match arg(1).as_deref() {
                        Some("FLOAT64") => E::F64,
                        Some("INT8") => E::I8,
                        Some("BINARY") => E::Bit,
                        _ => E::F32,
                    },
                    sparse: arg(2).as_deref() == Some("SPARSE"),
                }
            }
            "xmltype" | "sys.xmltype" => L::Xml,
            "sdo_geometry" | "mdsys.sdo_geometry" => L::Geometry {
                kind: None,
                srid: None,
                geography: false,
            },
            "rowid" => L::Varchar {
                len: Some(18),
                unicode: false,
            },
            "urowid" => L::Varchar {
                len: Some(p(0).unwrap_or(4000)),
                unicode: false,
            },
            // `INTERVAL DAY(2) TO SECOND(6)`, `INTERVAL YEAR(2) TO MONTH`.
            n if n.starts_with("interval") => L::Interval,
            _ => L::Other {
                native: t.raw.clone(),
            },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Vector { .. } => render_vector(t),
            L::Bool => Rendered::exact("NUMBER(1)").with(Info, TypeChanged, "Booleano como NUMBER(1) (Oracle recién tiene BOOLEAN desde 23ai)."),
            L::Int { bytes, unsigned } => {
                let digits = match (bytes, unsigned) {
                    (1, _) => 3,
                    (2, _) => 5,
                    (3, _) => 8,
                    (4, _) => 10,
                    (8, false) => 19,
                    (8, true) => 20,
                    _ => 38,
                };
                let r = Rendered::exact(format!("NUMBER({digits})"));
                if *bytes > 8 {
                    r.with(Loss, RangeLoss, "Entero de 16 bytes como NUMBER(38): no entran los valores de 39 dígitos.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } => {
                let s = scale.unwrap_or(0);
                let (np, ns, lossy) = fit_decimal(*p, s, 38, 38);
                let r = Rendered::exact(format!("NUMBER({np}, {ns})"));
                if lossy {
                    r.with(Loss, PrecisionLoss, format!("Oracle admite hasta 38 dígitos; el origen es ({p}, {s}): queda ({np}, {ns})."))
                } else {
                    r
                }
            }
            L::Decimal { precision: None, .. } => Rendered::exact("NUMBER"),
            L::Float { bytes: 4 } => Rendered::exact("BINARY_FLOAT"),
            L::Float { .. } => Rendered::exact("BINARY_DOUBLE"),
            L::Money => Rendered::exact("NUMBER(19, 4)").with(Info, TypeChanged, "Moneda como NUMBER(19, 4)."),
            L::Char { len, .. } => match len.unwrap_or(1) {
                n if n <= MAX_CHAR => multibyte(Rendered::exact(format!("CHAR({n} CHAR)")), n, MAX_CHAR),
                n if n <= MAX_VARCHAR2 => multibyte(Rendered::exact(format!("VARCHAR2({n} CHAR)")), n, MAX_VARCHAR2)
                    .with(Info, TypeChanged, "CHAR de Oracle admite hasta 2000 bytes: se usa VARCHAR2."),
                _ => Rendered::exact("CLOB").with(Info, TypeChanged, "Texto fijo de más de 4000 bytes: se usa CLOB."),
            },
            L::Varchar { len: Some(n), .. } if *n <= MAX_VARCHAR2 => multibyte(Rendered::exact(format!("VARCHAR2({n} CHAR)")), *n, MAX_VARCHAR2),
            L::Varchar { len, unicode } => {
                let r = Rendered::exact(if *unicode { "NCLOB" } else { "CLOB" });
                match len {
                    Some(n) => r.with(Info, TypeChanged, format!("varchar({n}) supera los 4000 bytes de VARCHAR2: se usa CLOB.")),
                    None => r,
                }
            }
            L::Text { unicode } => Rendered::exact(if *unicode { "NCLOB" } else { "CLOB" }),
            L::Binary { len } | L::Varbinary { len } => match len {
                Some(n) if *n <= 2000 => Rendered::exact(format!("RAW({n})")),
                _ => Rendered::exact("BLOB"),
            },
            L::Blob => Rendered::exact("BLOB"),
            L::Bit { len } => Rendered::exact(format!("RAW({})", len.map_or(2000, |n| n.div_ceil(8).max(1))))
                .with(Warning, TypeApproximated, "Oracle no tiene cadenas de bits: se guarda como RAW."),
            L::Date => Rendered::exact("DATE").with(Info, TypeChanged, "El DATE de Oracle también guarda la hora (queda en 00:00:00)."),
            L::Time { precision, tz } => {
                let r = Rendered::exact(format!("TIMESTAMP{}", prec(*precision, 9)))
                    .with(Warning, TypeApproximated, "Oracle no tiene tipo hora: se guarda como TIMESTAMP, con una fecha.");
                if *tz {
                    r.with(Loss, TimeZoneLoss, "La zona horaria de una hora no se guarda.")
                } else {
                    r
                }
            }
            L::Timestamp { precision: Some(0), tz: false } => Rendered::exact("DATE"),
            L::Timestamp { precision, tz } => Rendered::exact(format!("TIMESTAMP{}{}", prec(*precision, 9), if *tz { " WITH TIME ZONE" } else { "" }))
                .with_loss(precision_loss(*precision, 9)),
            L::Interval => Rendered::exact("INTERVAL DAY(9) TO SECOND(9)")
                .with(Warning, TypeApproximated, "INTERVAL DAY TO SECOND no guarda meses ni años: un intervalo que los tenga no entra."),
            L::Year => Rendered::exact("NUMBER(4)"),
            L::Uuid => Rendered::exact("RAW(16)").with(Info, TypeChanged, "UUID como RAW(16)."),
            L::Json { .. } => Rendered::exact("JSON").with(Info, TypeChanged, "El tipo JSON requiere Oracle 21c o posterior; en versiones anteriores, CLOB con IS JSON."),
            L::Xml => Rendered::exact("XMLTYPE"),
            L::Enum { values } => Rendered::exact(format!("VARCHAR2({} CHAR)", longest(values).min(4000)))
                .with(Warning, TypeApproximated, format!("Oracle no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => Rendered::exact(match set_len(values) {
                n if n <= 4000 => format!("VARCHAR2({n} CHAR)"),
                _ => "CLOB".into(),
            })
            .with(Warning, TypeApproximated, format!("Oracle no tiene conjuntos: queda como texto separado por comas. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } => Rendered::exact("JSON").with(Warning, TypeApproximated, "Oracle no tiene arreglos ni mapas en columnas: se guarda como JSON."),
            L::Geometry { .. } => Rendered::exact("SDO_GEOMETRY"),
            L::Inet => Rendered::exact("VARCHAR2(45 CHAR)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR2(17 CHAR)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("RAW(8)").with(Warning, TypeApproximated, "Oracle no tiene versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp => Some(match ty {
                L::Timestamp { precision: Some(0), tz: false } => "SYSDATE".into(),
                L::Date => "TRUNC(SYSDATE)".into(),
                L::Timestamp { tz: false, .. } | L::Time { .. } => "LOCALTIMESTAMP".into(),
                _ => "SYSTIMESTAMP".into(),
            }),
            DefaultValue::CurrentDate => Some("TRUNC(SYSDATE)".into()),
            DefaultValue::CurrentTime => Some("LOCALTIMESTAMP".into()),
            DefaultValue::NewUuid => match uuid_slot(ty) {
                UuidSlot::Native | UuidSlot::Binary => Some("SYS_GUID()".into()),
                UuidSlot::Text => Some(
                    "LOWER(REGEXP_REPLACE(RAWTOHEX(SYS_GUID()), '(.{8})(.{4})(.{4})(.{4})(.{12})', '\\1-\\2-\\3-\\4-\\5'))".into(),
                ),
                UuidSlot::None => None,
            },
            // In Oracle '' is NULL.
            DefaultValue::Text(s) if s.is_empty() => Some("NULL".into()),
            other => standard_default(other, ty, "SYSTIMESTAMP", Some("SYS_GUID()"), true),
        }
    }

    /// `CREATE VECTOR INDEX … ORGANIZATION INMEMORY NEIGHBOR GRAPH` (HNSW)
    /// or `NEIGHBOR PARTITIONS` (IVF); another family's graph index
    /// (DiskANN, HNSW variants) becomes HNSW, its list index IVF.
    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
    ) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
        use dbine_driver::vector::{metric, param};
        let algorithm = match v
            .algorithm
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("ivf" | "ivfflat" | "ivf_flat" | "ivf_pq" | "ivf_sq") => "ivf",
            _ => "hnsw",
        };
        let m = v.metric.as_deref().map(str::to_ascii_lowercase);
        let metric = match m.as_deref() {
            None => None,
            Some(
                x @ (metric::COSINE
                | metric::L2
                | metric::IP
                | metric::L1
                | metric::HAMMING
                | metric::JACCARD),
            ) => Some(x.to_string()),
            Some("euclidean") => Some(metric::L2.into()),
            Some("dot" | "inner_product") => Some(metric::IP.into()),
            Some("manhattan") => Some(metric::L1.into()),
            Some(other) => return Err(format!("Oracle no tiene la distancia «{other}»")),
        };
        let keep: &[&str] = if algorithm == "hnsw" {
            &[param::M, param::EF_CONSTRUCTION, "target_accuracy"]
        } else {
            &[param::LISTS, "target_accuracy"]
        };
        let params = v
            .params
            .iter()
            .filter(|(k, _)| keep.contains(&k.as_str()))
            .map(|(k, x)| (k.clone(), x.clone()))
            .collect();
        Ok((
            Some("VECTOR".into()),
            dbine_driver::VectorIndex {
                algorithm: Some(algorithm.into()),
                metric,
                params,
            },
        ))
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            on_delete: &["CASCADE", "SET NULL", "NO ACTION"],
            on_update: &[],
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 128,
            case: IdentCase::Upper,
        }
    }

    /// What CREATE TABLE refuses otherwise: one identity column, numeric;
    /// no LOB in a key; keys within the index size limit.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        single_auto_increment(t, report, "Oracle");
        let table = t.name.clone();
        for c in t.columns.iter_mut().filter(|c| c.auto_increment) {
            let ok = matches!(
                self.parse_type(&parse(&c.data_type)),
                L::Int { .. }
                    | L::Decimal {
                        scale: Some(0) | None,
                        ..
                    }
            );
            if !ok {
                report.push(
                    Severity::Loss,
                    IssueCode::RangeLoss,
                    &table,
                    Some(&c.name),
                    format!("Una columna de identidad de Oracle tiene que ser numérica: «{}» pasa a NUMBER(19).", c.data_type),
                );
                c.data_type = "NUMBER(19)".into();
            }
        }

        let mut keys: Vec<(Vec<String>, String)> = Vec::new();
        if let Some(pk) = &t.primary_key {
            keys.push((pk.columns.clone(), "la clave primaria".into()));
        }
        for ix in &t.indexes {
            keys.push((ix.columns.clone(), format!("el índice «{}»", ix.name)));
        }
        for (cols, what) in keys {
            let budget = MAX_KEY / cols.len().max(1) as u32;
            let chars = budget / BYTES_PER_CHAR;
            for name in &cols {
                let Some(c) = t.columns.iter_mut().find(|c| &c.name == name) else {
                    continue;
                };
                let to = match self.parse_type(&parse(&c.data_type)) {
                    L::Text { .. } => format!("VARCHAR2({chars} CHAR)"),
                    L::Varchar { len: Some(n), .. } | L::Char { len: Some(n), .. } if n > chars => {
                        format!("VARCHAR2({chars} CHAR)")
                    }
                    L::Blob => format!("RAW({})", budget.min(2000)),
                    _ => continue,
                };
                report.push(
                    Severity::Loss,
                    IssueCode::LengthLoss,
                    &table,
                    Some(name),
                    format!("Oracle no admite «{}» en {what} (hasta unos {MAX_KEY} bytes): pasa a {to}.", c.data_type),
                );
                c.data_type = to;
            }
        }
    }
}

/// A note when `n` characters may not fit `max_bytes` (up to 4 bytes each).
fn multibyte(r: Rendered, n: u32, max_bytes: u32) -> Rendered {
    if n * BYTES_PER_CHAR > max_bytes {
        r.with(
            Severity::Warning,
            IssueCode::LengthLoss,
            format!("Oracle limita la columna a {max_bytes} bytes: con caracteres de varios bytes pueden entrar menos de {n} caracteres."),
        )
    } else {
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(s: &str) -> L {
        Oracle.parse_type(&parse(s))
    }

    #[test]
    fn parses_dictionary_spellings() {
        assert_eq!(ty("NUMBER(10)"), L::int(8));
        assert_eq!(ty("NUMBER(9)"), L::int(4));
        assert_eq!(
            ty("NUMBER(12,2)"),
            L::Decimal {
                precision: Some(12),
                scale: Some(2)
            }
        );
        assert_eq!(
            ty("NUMBER"),
            L::Decimal {
                precision: None,
                scale: None
            }
        );
        assert_eq!(
            ty("INTEGER"),
            L::Decimal {
                precision: Some(38),
                scale: Some(0)
            }
        );
        assert_eq!(ty("NUMBER(10,-2)"), L::int(8));
        assert_eq!(
            ty("NUMBER(*,0)"),
            L::Decimal {
                precision: Some(38),
                scale: Some(0)
            }
        );
        assert_eq!(
            ty("FLOAT(126)"),
            L::Decimal {
                precision: None,
                scale: None
            }
        );
        assert_eq!(ty("FLOAT(53)"), L::Float { bytes: 8 });
        assert_eq!(
            ty("VARCHAR2(200)"),
            L::Varchar {
                len: Some(200),
                unicode: true
            }
        );
        assert_eq!(
            ty("VARCHAR2(100 CHAR)"),
            L::Varchar {
                len: Some(100),
                unicode: true
            }
        );
        assert_eq!(
            ty("NVARCHAR2(50)"),
            L::Varchar {
                len: Some(50),
                unicode: true
            }
        );
        assert_eq!(
            ty("CHAR(10)"),
            L::Char {
                len: Some(10),
                unicode: true
            }
        );
        assert_eq!(ty("RAW(16)"), L::Varbinary { len: Some(16) });
        assert_eq!(
            ty("DATE"),
            L::Timestamp {
                precision: Some(0),
                tz: false
            }
        );
        assert_eq!(
            ty("TIMESTAMP(6)"),
            L::Timestamp {
                precision: Some(6),
                tz: false
            }
        );
        assert_eq!(
            ty("TIMESTAMP(9) WITH TIME ZONE"),
            L::Timestamp {
                precision: Some(9),
                tz: true
            }
        );
        assert_eq!(
            ty("TIMESTAMP(6) WITH LOCAL TIME ZONE"),
            L::Timestamp {
                precision: Some(6),
                tz: true
            }
        );
        assert_eq!(ty("INTERVAL DAY(2) TO SECOND(6)"), L::Interval);
        assert_eq!(ty("INTERVAL YEAR(2) TO MONTH"), L::Interval);
        assert_eq!(ty("BINARY_FLOAT"), L::Float { bytes: 4 });
        assert_eq!(ty("LONG RAW"), L::Blob);
        assert_eq!(ty("BOOLEAN"), L::Bool);
        assert_eq!(ty("JSON"), L::Json { binary: true });
        assert_eq!(
            ty("NUMBER(10) GENERATED ALWAYS AS (\"A\"+1) VIRTUAL"),
            L::int(8)
        );
    }

    #[test]
    fn renders_character_semantics_within_limits() {
        assert_eq!(
            Oracle
                .render_type(&L::Varchar {
                    len: Some(200),
                    unicode: true
                })
                .native,
            "VARCHAR2(200 CHAR)"
        );
        let r = Oracle.render_type(&L::Varchar {
            len: Some(4000),
            unicode: true,
        });
        assert_eq!(r.native, "VARCHAR2(4000 CHAR)");
        assert_eq!(r.notes[0].code, IssueCode::LengthLoss);
        assert_eq!(
            Oracle
                .render_type(&L::Char {
                    len: Some(10),
                    unicode: true
                })
                .native,
            "CHAR(10 CHAR)"
        );
        assert_eq!(
            Oracle
                .render_type(&L::Decimal {
                    precision: Some(65),
                    scale: Some(30)
                })
                .native,
            "NUMBER(38, 3)"
        );
    }

    #[test]
    fn defaults_by_column_type() {
        assert_eq!(
            Oracle
                .render_default(&DefaultValue::CurrentTimestamp, &L::Date)
                .as_deref(),
            Some("TRUNC(SYSDATE)")
        );
        assert_eq!(
            Oracle
                .render_default(&DefaultValue::NewUuid, &L::Varbinary { len: Some(16) })
                .as_deref(),
            Some("SYS_GUID()")
        );
        assert!(Oracle
            .render_default(
                &DefaultValue::NewUuid,
                &L::Char {
                    len: Some(36),
                    unicode: true
                }
            )
            .unwrap()
            .starts_with("LOWER("));
    }

    #[test]
    fn vectors_round_trip() {
        use dbine_driver::{VectorElement as E, VectorIndex};
        let v = |dims, element, sparse| L::Vector {
            dims,
            element,
            sparse,
        };
        assert_eq!(ty("VECTOR(3, FLOAT32)"), v(Some(3), E::F32, false));
        assert_eq!(ty("VECTOR(*, *)"), v(None, E::F32, false));
        assert_eq!(ty("VECTOR(16, BINARY)"), v(Some(16), E::Bit, false));
        assert_eq!(ty("VECTOR(5, FLOAT32, SPARSE)"), v(Some(5), E::F32, true));
        assert_eq!(ty("VECTOR(2, FLOAT64)"), v(Some(2), E::F64, false));
        for native in [
            "VECTOR(3, FLOAT32)",
            "VECTOR(16, BINARY)",
            "VECTOR(4, INT8)",
            "VECTOR(2, FLOAT64)",
            "VECTOR(5, FLOAT32, SPARSE)",
            "VECTOR(*, FLOAT32)",
        ] {
            assert_eq!(Oracle.render_type(&ty(native)).native, native);
        }
        // pgvector's vector(3), halfvec(3), bit(12), sparsevec(5).
        assert_eq!(
            Oracle.render_type(&v(Some(3), E::F32, false)).native,
            "VECTOR(3, FLOAT32)"
        );
        let half = Oracle.render_type(&v(Some(3), E::F16, false));
        assert_eq!(half.native, "VECTOR(3, FLOAT32)");
        assert!(!half.notes.is_empty());
        assert_eq!(
            Oracle.render_type(&v(Some(12), E::Bit, false)).native,
            "VECTOR(12, INT8)"
        );
        assert_eq!(
            Oracle.render_type(&v(Some(5), E::F32, true)).native,
            "VECTOR(5, FLOAT32, SPARSE)"
        );
        // Indexes: HNSW and IVF keep their parameters; other graphs become HNSW.
        let hnsw = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("ip".into()),
            params: [
                ("m".to_string(), "16".to_string()),
                ("ef_construction".to_string(), "64".to_string()),
                ("lists".to_string(), "9".to_string()),
            ]
            .into(),
        };
        let (kind, m) = Oracle.vector_index(&hnsw).unwrap();
        assert_eq!(kind.as_deref(), Some("VECTOR"));
        assert_eq!(m.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(m.params.len(), 2);
        let ivf = VectorIndex {
            algorithm: Some("ivfflat".into()),
            metric: Some("l2".into()),
            params: [("lists".to_string(), "100".to_string())].into(),
        };
        let (_, m) = Oracle.vector_index(&ivf).unwrap();
        assert_eq!(m.algorithm.as_deref(), Some("ivf"));
        assert_eq!(m.params.get("lists").map(String::as_str), Some("100"));
        let diskann = VectorIndex {
            algorithm: Some("diskann".into()),
            ..Default::default()
        };
        assert_eq!(
            Oracle
                .vector_index(&diskann)
                .unwrap()
                .1
                .algorithm
                .as_deref(),
            Some("hnsw")
        );
        let odd = VectorIndex {
            metric: Some("chebyshev".into()),
            ..Default::default()
        };
        assert!(Oracle.vector_index(&odd).is_err());
    }
}
