//! Firebird (3, 4 and 5; dialect 3 databases).
//!
//! The driver rebuilds the type from `RDB$FIELDS` (`INTEGER`,
//! `NUMERIC(18,2)`, `VARCHAR(50)`, `BLOB SUB_TYPE TEXT`,
//! `TIMESTAMP WITH TIME ZONE`…). Text lengths are characters; the
//! character set isn't reported, so text is taken as Unicode (the driver
//! connects in UTF8, the usual database charset).
//!
//! What needs Firebird 4 or later: INT128, DECFLOAT, NUMERIC above 18
//! digits, BINARY/VARBINARY and the time zone types. Firebird 5 adds
//! partial indexes.

use super::postgres::{longest, precision_loss};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct Firebird;

/// CHAR/VARCHAR hold 32767/32765 bytes: 8191 characters in UTF8.
const MAX_CHARS: u32 = 8191;
const MAX_BYTES: u32 = 32765;
const MAX_DECIMAL: u32 = 38;
/// Fractions of a second: 1/10000.
const TIME_PRECISION: u8 = 4;
const TEXT_BLOB: &str = "BLOB SUB_TYPE TEXT";
const BINARY_BLOB: &str = "BLOB SUB_TYPE BINARY";

impl Dialect for Firebird {
    fn id(&self) -> &'static str {
        "firebird"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let octets = t.rest.iter().any(|r| r == "octets");
        match t.name.as_str() {
            "boolean" => L::Bool,
            "smallint" => L::int(2),
            "integer" | "int" => L::int(4),
            "bigint" => L::int(8),
            "int128" => L::int(16),
            "numeric" | "decimal" | "dec" => L::Decimal { precision: p(0).or(Some(18)), scale: p(1).or(Some(0)) },
            // Decimal floating point: 16 or 34 significant digits.
            "decfloat" => L::Decimal { precision: None, scale: None },
            "float" if p(0).is_some_and(|b| b > 24) => L::Float { bytes: 8 },
            "float" | "real" => L::Float { bytes: 4 },
            "double precision" | "double" => L::Float { bytes: 8 },
            "char" | "character" if octets => L::Binary { len: p(0).or(Some(1)) },
            "varchar" | "character varying" | "char varying" if octets => L::Varbinary { len: p(0) },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "character varying" | "char varying" | "cstring" => L::Varchar { len: p(0), unicode: true },
            "nchar" | "national character" | "national char" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "national character varying" | "national char varying" | "nchar varying" => L::Varchar { len: p(0), unicode: true },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" | "binary varying" => L::Varbinary { len: p(0) },
            "blob sub_type text" | "blob sub_type 1" => L::Text { unicode: true },
            n if n == "blob" || n.starts_with("blob sub_type") => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: Some(TIME_PRECISION), tz: t.with_tz },
            "timestamp" => L::Timestamp { precision: Some(TIME_PRECISION), tz: t.with_tz },
            // `COMPUTED BY (…)`, `/* type n */`, domains.
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => {
                let r = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 | 2 => Rendered::exact("SMALLINT"),
                    3 | 4 => Rendered::exact("INTEGER"),
                    8 => Rendered::exact("BIGINT"),
                    _ => Rendered::exact("INT128"),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, "Firebird no tiene enteros sin signo: se usa un tipo más grande con signo.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } if *p <= MAX_DECIMAL => {
                let r = Rendered::exact(format!("NUMERIC({p}, {})", scale.unwrap_or(0)));
                if *p > 18 {
                    r.with(Info, TypeChanged, "NUMERIC de más de 18 dígitos requiere Firebird 4 o posterior.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), .. } if *p <= 34 => Rendered::exact("DECFLOAT(34)")
                .with(Info, TypeChanged, format!("NUMERIC de {p} dígitos supera los {MAX_DECIMAL} de Firebird: se usa DECFLOAT(34), que los guarda exactos.")),
            L::Decimal { precision: Some(p), .. } => Rendered::exact("DECFLOAT(34)")
                .with(Loss, PrecisionLoss, format!("Firebird guarda hasta 34 dígitos significativos (DECFLOAT); el origen tiene {p}.")),
            L::Decimal { precision: None, .. } => Rendered::exact("DECFLOAT(34)")
                .with(Loss, PrecisionLoss, "Número sin precisión fija como DECFLOAT(34): hasta 34 dígitos significativos."),
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE PRECISION"),
            L::Money => Rendered::exact("NUMERIC(19, 4)").with(Info, TypeChanged, "Moneda como NUMERIC(19, 4)."),
            L::Char { len, .. } => match len.unwrap_or(1) {
                n if n <= MAX_CHARS => Rendered::exact(format!("CHAR({n})")),
                n => Rendered::exact(TEXT_BLOB).with(Info, TypeChanged, format!("CHAR({n}) supera los {MAX_CHARS} caracteres de Firebird en UTF8: se usa BLOB de texto.")),
            },
            L::Varchar { len: Some(n), .. } if *n <= MAX_CHARS => Rendered::exact(format!("VARCHAR({n})")),
            L::Varchar { len: Some(n), .. } => Rendered::exact(TEXT_BLOB)
                .with(Info, TypeChanged, format!("VARCHAR({n}) supera los {MAX_CHARS} caracteres de Firebird en UTF8: se usa BLOB de texto.")),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact(TEXT_BLOB),
            L::Binary { len } => match len.unwrap_or(1) {
                n if n <= MAX_BYTES => Rendered::exact(format!("BINARY({n})")),
                _ => Rendered::exact(BINARY_BLOB),
            },
            L::Varbinary { len: Some(n) } if *n <= MAX_BYTES => Rendered::exact(format!("VARBINARY({n})")),
            L::Varbinary { .. } | L::Blob => Rendered::exact(BINARY_BLOB),
            L::Bit { len: Some(1) } => Rendered::exact("BOOLEAN"),
            L::Bit { len } => self.render_type(&L::Varbinary { len: len.map(|n| n.div_ceil(8)) })
                .with(Warning, TypeApproximated, "Firebird no tiene cadenas de bits: se guardan como binario."),
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => Rendered::exact(if *tz { "TIME WITH TIME ZONE" } else { "TIME" })
                .with_loss(precision_loss(*precision, TIME_PRECISION)),
            L::Timestamp { precision, tz } => Rendered::exact(if *tz { "TIMESTAMP WITH TIME ZONE" } else { "TIMESTAMP" })
                .with_loss(precision_loss(*precision, TIME_PRECISION)),
            L::Interval => Rendered::exact("VARCHAR(100)").with(Warning, TypeApproximated, "Firebird no tiene intervalos: se guardan como texto."),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            // GEN_UUID() returns these 16 bytes.
            L::Uuid => Rendered::exact("BINARY(16)").with(Info, TypeChanged, "UUID como BINARY(16), el formato de GEN_UUID()."),
            L::Json { .. } => Rendered::exact(TEXT_BLOB).with(Info, TypeChanged, "Firebird no tiene tipo JSON: se guarda como BLOB de texto."),
            L::Xml => Rendered::exact(TEXT_BLOB).with(Info, TypeChanged, "Firebird no tiene tipo XML: se guarda como BLOB de texto."),
            L::Enum { values } | L::Set { values } => Rendered::exact(format!("VARCHAR({})", longest(values)))
                .with(Warning, TypeApproximated, format!("Firebird no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } => Rendered::exact(TEXT_BLOB)
                .with(Warning, TypeApproximated, "Firebird no tiene arreglos ni mapas en SQL: se guardan como JSON en un BLOB de texto."),
            L::Geometry { .. } => Rendered::exact(BINARY_BLOB)
                .with(Warning, TypeApproximated, "Firebird no tiene tipos espaciales: se guardan como binario (WKB)."),
            L::Inet => Rendered::exact("VARCHAR(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("BINARY(8)")
                .with(Warning, TypeApproximated, "Firebird no tiene versión de fila automática: queda como binario y no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        // A Firebird DEFAULT is a literal, NULL or a context variable: no
        // function calls (so no GEN_UUID()).
        match d {
            DefaultValue::CurrentTimestamp => Some(match ty {
                L::Date => "CURRENT_DATE".into(),
                L::Time { tz: true, .. } => "CURRENT_TIME".into(),
                L::Time { .. } => "LOCALTIME".into(),
                L::Timestamp { tz: true, .. } => "CURRENT_TIMESTAMP".into(),
                _ => "LOCALTIMESTAMP".into(),
            }),
            DefaultValue::CurrentDate => Some("CURRENT_DATE".into()),
            DefaultValue::CurrentTime => Some(if matches!(ty, L::Time { tz: true, .. }) { "CURRENT_TIME" } else { "LOCALTIME" }.into()),
            // Text defaults can't go on a BLOB.
            DefaultValue::Text(_) if matches!(self.render_type(ty).native.as_str(), TEXT_BLOB | BINARY_BLOB) => None,
            other => standard_default(other, ty, "LOCALTIMESTAMP", None, false),
        }
    }

    fn caps(&self) -> Caps {
        const ACTIONS: &[&str] = &["CASCADE", "SET NULL", "SET DEFAULT", "NO ACTION"];
        Caps {
            foreign_keys: true,
            on_delete: ACTIONS,
            on_update: ACTIONS,
            indexes: true,
            // Firebird 5.
            partial_indexes: true,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: true,
            // 63 characters since Firebird 4 (31 before).
            max_identifier: 63,
            case: IdentCase::Upper,
        }
    }

    /// Firebird (before 6) has no schemas: the source's schema would make
    /// `"public"."T"` a syntax error.
    fn finalize(&self, t: &mut TableSchema, _report: &mut Report) {
        t.schema = None;
        for fk in &mut t.foreign_keys {
            fk.ref_schema = None;
        }
    }
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Firebird = Firebird;
    (driver_id == "firebird").then_some(&D as &dyn Dialect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn p(s: &str) -> L {
        Firebird.parse_type(&parse(s))
    }
    fn r(t: L) -> String {
        Firebird.render_type(&t).native
    }

    #[test]
    fn parses_what_the_driver_reports() {
        assert_eq!(p("SMALLINT"), L::int(2));
        assert_eq!(p("INTEGER"), L::int(4));
        assert_eq!(p("BIGINT"), L::int(8));
        assert_eq!(p("INT128"), L::int(16));
        assert_eq!(p("NUMERIC(18,2)"), L::Decimal { precision: Some(18), scale: Some(2) });
        assert_eq!(p("DECIMAL(9,3)"), L::Decimal { precision: Some(9), scale: Some(3) });
        assert_eq!(p("DECFLOAT(34)"), L::Decimal { precision: None, scale: None });
        assert_eq!(p("FLOAT"), L::Float { bytes: 4 });
        assert_eq!(p("DOUBLE PRECISION"), L::Float { bytes: 8 });
        assert_eq!(p("CHAR(10)"), L::Char { len: Some(10), unicode: true });
        assert_eq!(p("VARCHAR(50)"), L::Varchar { len: Some(50), unicode: true });
        assert_eq!(p("CSTRING(20)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(p("CHAR(16) CHARACTER SET OCTETS"), L::Binary { len: Some(16) });
        assert_eq!(p("BINARY(16)"), L::Binary { len: Some(16) });
        assert_eq!(p("VARBINARY(100)"), L::Varbinary { len: Some(100) });
        assert_eq!(p("BLOB SUB_TYPE TEXT"), L::Text { unicode: true });
        assert_eq!(p("BLOB SUB_TYPE BINARY"), L::Blob);
        assert_eq!(p("BLOB SUB_TYPE 5"), L::Blob);
        assert_eq!(p("BLOB"), L::Blob);
        assert_eq!(p("BOOLEAN"), L::Bool);
        assert_eq!(p("DATE"), L::Date);
        assert_eq!(p("TIME"), L::Time { precision: Some(4), tz: false });
        assert_eq!(p("TIME WITH TIME ZONE"), L::Time { precision: Some(4), tz: true });
        assert_eq!(p("TIMESTAMP"), L::Timestamp { precision: Some(4), tz: false });
        assert_eq!(p("TIMESTAMP WITH TIME ZONE"), L::Timestamp { precision: Some(4), tz: true });
        assert!(matches!(p("COMPUTED BY (ID * 2)"), L::Other { .. }));
        assert!(matches!(p("/* type 99 */"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::Int { bytes: 1, unsigned: true }), "SMALLINT");
        assert_eq!(r(L::int(4)), "INTEGER");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "BIGINT");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "INT128");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "NUMERIC(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(38), scale: Some(0) }), "NUMERIC(38, 0)");
        assert_eq!(r(L::Decimal { precision: Some(40), scale: Some(2) }), "DECFLOAT(34)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "DECFLOAT(34)");
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Float { bytes: 8 }), "DOUBLE PRECISION");
        assert_eq!(r(L::Money), "NUMERIC(19, 4)");
        assert_eq!(r(L::Char { len: None, unicode: false }), "CHAR(1)");
        assert_eq!(r(L::Char { len: Some(9000), unicode: true }), TEXT_BLOB);
        assert_eq!(r(L::Varchar { len: Some(8191), unicode: true }), "VARCHAR(8191)");
        assert_eq!(r(L::Varchar { len: Some(8192), unicode: true }), TEXT_BLOB);
        assert_eq!(r(L::Text { unicode: true }), TEXT_BLOB);
        assert_eq!(r(L::Binary { len: Some(16) }), "BINARY(16)");
        assert_eq!(r(L::Varbinary { len: Some(100) }), "VARBINARY(100)");
        assert_eq!(r(L::Varbinary { len: None }), BINARY_BLOB);
        assert_eq!(r(L::Blob), BINARY_BLOB);
        assert_eq!(r(L::Bit { len: Some(1) }), "BOOLEAN");
        assert_eq!(r(L::Bit { len: Some(10) }), "VARBINARY(2)");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: Some(6), tz: true }), "TIME WITH TIME ZONE");
        assert!(Firebird.render_type(&L::Time { precision: Some(6), tz: false }).notes.iter().any(|n| n.code == IssueCode::PrecisionLoss));
        assert_eq!(r(L::Timestamp { precision: Some(3), tz: false }), "TIMESTAMP");
        assert!(Firebird.render_type(&L::Timestamp { precision: Some(3), tz: false }).notes.is_empty());
        assert_eq!(r(L::Timestamp { precision: None, tz: true }), "TIMESTAMP WITH TIME ZONE");
        assert_eq!(r(L::Interval), "VARCHAR(100)");
        assert_eq!(r(L::Year), "SMALLINT");
        assert_eq!(r(L::Uuid), "BINARY(16)");
        assert_eq!(r(L::Json { binary: true }), TEXT_BLOB);
        assert_eq!(r(L::Xml), TEXT_BLOB);
        assert_eq!(r(L::Enum { values: vec!["uno".into(), "cuatro".into()] }), "VARCHAR(6)");
        assert_eq!(r(L::Set { values: vec!["a".into()] }), "VARCHAR(1)");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }), TEXT_BLOB);
        assert_eq!(r(L::Map { key: Box::new(L::int(4)), value: Box::new(L::int(4)) }), TEXT_BLOB);
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), BINARY_BLOB);
        assert_eq!(r(L::Inet), "VARCHAR(45)");
        assert_eq!(r(L::MacAddr), "VARCHAR(17)");
        assert_eq!(r(L::RowVersion), "BINARY(8)");
        assert_eq!(r(L::Other { native: "DOM_X".into() }), "DOM_X");
    }

    #[test]
    fn round_trips_its_own_spellings() {
        for t in [
            L::Bool,
            L::int(2),
            L::int(4),
            L::int(8),
            L::int(16),
            L::Decimal { precision: Some(18), scale: Some(2) },
            L::Float { bytes: 4 },
            L::Float { bytes: 8 },
            L::Char { len: Some(3), unicode: true },
            L::Varchar { len: Some(30), unicode: true },
            L::Text { unicode: true },
            L::Binary { len: Some(16) },
            L::Blob,
            L::Date,
            L::Time { precision: Some(4), tz: true },
            L::Timestamp { precision: Some(4), tz: false },
        ] {
            let native = r(t.clone());
            assert_eq!(p(&native), t, "{native}");
        }
    }

    #[test]
    fn defaults() {
        let d = |v: DefaultValue, t: L| Firebird.render_default(&v, &t);
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: None, tz: false }).as_deref(), Some("LOCALTIMESTAMP"));
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: None, tz: true }).as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(d(DefaultValue::CurrentDate, L::Date).as_deref(), Some("CURRENT_DATE"));
        assert_eq!(d(DefaultValue::CurrentTime, L::Time { precision: None, tz: false }).as_deref(), Some("LOCALTIME"));
        assert_eq!(d(DefaultValue::Bool(false), L::Bool).as_deref(), Some("FALSE"));
        assert_eq!(d(DefaultValue::NewUuid, L::Uuid), None);
        assert_eq!(d(DefaultValue::Text("x".into()), L::Text { unicode: true }), None);
        assert_eq!(d(DefaultValue::Text("x".into()), L::Varchar { len: Some(5), unicode: true }).as_deref(), Some("'x'"));
        assert_eq!(d(DefaultValue::Number("1.5".into()), L::Decimal { precision: Some(5), scale: Some(2) }).as_deref(), Some("1.5"));
    }
}
