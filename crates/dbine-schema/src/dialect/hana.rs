//! SAP HANA (HANA 2 and HANA Cloud).
//!
//! The driver reads `SYS.TABLE_COLUMNS.DATA_TYPE_NAME` and adds the length
//! (`NVARCHAR(100)`, `VARBINARY(16)`) or precision and scale
//! (`DECIMAL(12,2)`; a bare `DECIMAL` is the floating decimal).
//! Everything textual is written as NVARCHAR/NCLOB: in HANA 2, VARCHAR
//! only guarantees 7-bit ASCII.

use super::postgres::{longest, precision_loss};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;

pub struct Hana;

const MAX_NVARCHAR: u32 = 5000;
const MAX_NCHAR: u32 = 2000;
const MAX_VARBINARY: u32 = 5000;
const MAX_BINARY: u32 = 2000;
const MAX_DECIMAL: u32 = 38;
/// TIMESTAMP keeps 100 ns.
const TIMESTAMP_PRECISION: u8 = 7;

impl Dialect for Hana {
    fn id(&self) -> &'static str {
        "hana"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "boolean" => L::Bool,
            // HANA TINYINT is 0–255.
            "tinyint" => L::Int { bytes: 1, unsigned: true },
            "smallint" => L::int(2),
            "integer" | "int" => L::int(4),
            "bigint" => L::int(8),
            // DECIMAL without precision: floating decimal, 34 digits.
            "decimal" | "dec" | "numeric" => match p(0) {
                Some(pr) => L::Decimal { precision: Some(pr), scale: p(1).or(Some(0)) },
                None => L::Decimal { precision: None, scale: None },
            },
            "smalldecimal" => L::Decimal { precision: None, scale: None },
            "real" => L::Float { bytes: 4 },
            "double" | "double precision" => L::Float { bytes: 8 },
            "float" => L::Float { bytes: if p(0).is_some_and(|b| b <= 24) { 4 } else { 8 } },
            "nchar" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "nvarchar" | "shorttext" => L::Varchar { len: p(0), unicode: true },
            // HANA 2 VARCHAR/CHAR/ALPHANUM: 7-bit ASCII (HANA Cloud aliases
            // VARCHAR to NVARCHAR; taking the narrower reading is safe).
            "char" => L::Char { len: p(0).or(Some(1)), unicode: false },
            "varchar" | "alphanum" => L::Varchar { len: p(0), unicode: false },
            "nclob" | "text" | "bintext" => L::Text { unicode: true },
            "clob" => L::Text { unicode: false },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" => L::Varbinary { len: p(0) },
            "blob" => L::Blob,
            "date" | "daydate" => L::Date,
            "time" | "secondtime" => L::Time { precision: Some(0), tz: false },
            "seconddate" => L::Timestamp { precision: Some(0), tz: false },
            "timestamp" | "longdate" => L::Timestamp { precision: Some(TIMESTAMP_PRECISION), tz: false },
            "st_geometry" | "st_point" => L::Geometry {
                kind: (t.name == "st_point").then(|| "point".to_string()),
                srid: p(0),
                geography: false,
            },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes: 1, unsigned: true } => Rendered::exact("TINYINT"),
            L::Int { bytes, unsigned } => {
                let r = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 | 2 => Rendered::exact("SMALLINT"),
                    3 | 4 => Rendered::exact("INTEGER"),
                    8 => Rendered::exact("BIGINT"),
                    _ if *bytes == 8 => Rendered::exact("DECIMAL(20, 0)"),
                    _ => Rendered::exact("DECIMAL(38, 0)").with(Loss, RangeLoss, "Entero de 16 bytes como DECIMAL(38, 0): no entran los valores de 39 dígitos."),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, "HANA no tiene enteros sin signo (salvo TINYINT): se usa un tipo más grande con signo.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } if *p <= MAX_DECIMAL => Rendered::exact(format!("DECIMAL({p}, {})", scale.unwrap_or(0))),
            L::Decimal { precision: Some(p), .. } => Rendered::exact("DECIMAL")
                .with(Loss, PrecisionLoss, format!("HANA admite hasta {MAX_DECIMAL} dígitos fijos; el origen tiene {p}: se usa DECIMAL flotante (34 dígitos significativos).")),
            L::Decimal { precision: None, .. } => Rendered::exact("DECIMAL")
                .with(Info, TypeChanged, "Número sin precisión fija como DECIMAL flotante de HANA: hasta 34 dígitos significativos."),
            L::Float { bytes: 4 } => Rendered::exact("REAL"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(Info, TypeChanged, "Moneda como DECIMAL(19, 4)."),
            L::Char { len, .. } => match len.unwrap_or(1) {
                n if n <= MAX_NCHAR => Rendered::exact(format!("NCHAR({n})")),
                n if n <= MAX_NVARCHAR => Rendered::exact(format!("NVARCHAR({n})")).with(Info, TypeChanged, format!("NCHAR admite hasta {MAX_NCHAR}: se usa NVARCHAR.")),
                n => Rendered::exact("NCLOB").with(Info, TypeChanged, format!("CHAR({n}) supera los {MAX_NVARCHAR} de NVARCHAR: se usa NCLOB.")),
            },
            L::Varchar { len: Some(n), .. } if *n <= MAX_NVARCHAR => Rendered::exact(format!("NVARCHAR({n})")),
            L::Varchar { len: Some(n), .. } => Rendered::exact("NCLOB")
                .with(Info, TypeChanged, format!("VARCHAR({n}) supera los {MAX_NVARCHAR} de NVARCHAR: se usa NCLOB.")),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact("NCLOB"),
            L::Binary { len } => match len.unwrap_or(1) {
                n if n <= MAX_BINARY => Rendered::exact(format!("BINARY({n})")),
                n if n <= MAX_VARBINARY => Rendered::exact(format!("VARBINARY({n})")),
                _ => Rendered::exact("BLOB"),
            },
            L::Varbinary { len: Some(n) } if *n <= MAX_VARBINARY => Rendered::exact(format!("VARBINARY({n})")),
            L::Varbinary { .. } | L::Blob => Rendered::exact("BLOB"),
            L::Bit { len: Some(1) } => Rendered::exact("BOOLEAN"),
            L::Bit { len } => self.render_type(&L::Varbinary { len: len.map(|n| n.div_ceil(8)) })
                .with(Warning, TypeApproximated, "HANA no tiene cadenas de bits: se guardan como binario."),
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => {
                let mut r = Rendered::exact("TIME");
                if precision.is_some_and(|p| p > 0) {
                    r = r.with(Loss, PrecisionLoss, "El TIME de HANA no guarda fracciones de segundo.");
                }
                if *tz {
                    r = r.with(Loss, TimeZoneLoss, "HANA no guarda la zona horaria de una hora.");
                }
                r
            }
            L::Timestamp { precision, tz } => {
                let r = match precision {
                    Some(0) => Rendered::exact("SECONDDATE"),
                    _ => Rendered::exact("TIMESTAMP").with_loss(precision_loss(*precision, TIMESTAMP_PRECISION)),
                };
                if *tz {
                    r.with(Loss, TimeZoneLoss, "HANA no guarda la zona horaria: conviene convertir los valores a UTC al copiarlos.")
                } else {
                    r
                }
            }
            L::Interval => Rendered::exact("NVARCHAR(100)").with(Warning, TypeApproximated, "HANA no tiene intervalos: se guardan como texto."),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            // NEWUID() / SYSUUID return 16 bytes.
            L::Uuid => Rendered::exact("VARBINARY(16)").with(Info, TypeChanged, "UUID como VARBINARY(16), el formato de SYSUUID."),
            L::Json { .. } => Rendered::exact("NCLOB").with(Info, TypeChanged, "HANA no tiene columnas JSON (solo colecciones del JSON Document Store): se guarda como NCLOB."),
            L::Xml => Rendered::exact("NCLOB").with(Info, TypeChanged, "HANA no tiene tipo XML: se guarda como NCLOB."),
            L::Enum { values } | L::Set { values } => Rendered::exact(format!("NVARCHAR({})", longest(values)))
                .with(Warning, TypeApproximated, format!("HANA no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } => Rendered::exact("NCLOB")
                .with(Warning, TypeApproximated, "Arreglos y mapas se guardan como JSON en un NCLOB."),
            L::Geometry { kind, srid, geography } => {
                let srid = srid.or(geography.then_some(4326));
                let base = if kind.as_deref() == Some("point") { "ST_POINT" } else { "ST_GEOMETRY" };
                Rendered::exact(match srid {
                    Some(s) => format!("{base}({s})"),
                    None => base.to_string(),
                })
            }
            L::Inet => Rendered::exact("NVARCHAR(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("NVARCHAR(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("VARBINARY(8)")
                .with(Warning, TypeApproximated, "HANA no tiene versión de fila automática: queda como binario y no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        // HANA DEFAULT: literals, NULL and the CURRENT_* functions.
        match d {
            DefaultValue::CurrentTimestamp => Some(match ty {
                L::Date => "CURRENT_DATE".into(),
                L::Time { .. } => "CURRENT_TIME".into(),
                _ => "CURRENT_TIMESTAMP".into(),
            }),
            DefaultValue::NewUuid => None,
            DefaultValue::Text(_) if matches!(ty, L::Text { .. } | L::Json { .. } | L::Xml | L::Array { .. } | L::Map { .. }) => None,
            other => standard_default(other, ty, "CURRENT_TIMESTAMP", None, false),
        }
    }

    fn caps(&self) -> Caps {
        // No NO ACTION keyword: RESTRICT is the default.
        const ACTIONS: &[&str] = &["CASCADE", "SET NULL", "SET DEFAULT", "RESTRICT"];
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
            max_identifier: 127,
            case: IdentCase::Upper,
        }
    }
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Hana = Hana;
    (driver_id == "hana").then_some(&D as &dyn Dialect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn p(s: &str) -> L {
        Hana.parse_type(&parse(s))
    }
    fn r(t: L) -> String {
        Hana.render_type(&t).native
    }

    #[test]
    fn parses_catalog_spellings() {
        assert_eq!(p("TINYINT"), L::Int { bytes: 1, unsigned: true });
        assert_eq!(p("SMALLINT"), L::int(2));
        assert_eq!(p("INTEGER"), L::int(4));
        assert_eq!(p("BIGINT"), L::int(8));
        assert_eq!(p("DECIMAL(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(p("DECIMAL"), L::Decimal { precision: None, scale: None });
        assert_eq!(p("SMALLDECIMAL"), L::Decimal { precision: None, scale: None });
        assert_eq!(p("REAL"), L::Float { bytes: 4 });
        assert_eq!(p("DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(p("NVARCHAR(100)"), L::Varchar { len: Some(100), unicode: true });
        assert_eq!(p("NCHAR(3)"), L::Char { len: Some(3), unicode: true });
        assert_eq!(p("VARCHAR(10)"), L::Varchar { len: Some(10), unicode: false });
        assert_eq!(p("ALPHANUM(10)"), L::Varchar { len: Some(10), unicode: false });
        assert_eq!(p("SHORTTEXT(10)"), L::Varchar { len: Some(10), unicode: true });
        assert_eq!(p("NCLOB"), L::Text { unicode: true });
        assert_eq!(p("CLOB"), L::Text { unicode: false });
        assert_eq!(p("TEXT"), L::Text { unicode: true });
        assert_eq!(p("VARBINARY(16)"), L::Varbinary { len: Some(16) });
        assert_eq!(p("BINARY(8)"), L::Binary { len: Some(8) });
        assert_eq!(p("BLOB"), L::Blob);
        assert_eq!(p("BOOLEAN"), L::Bool);
        assert_eq!(p("DATE"), L::Date);
        assert_eq!(p("TIME"), L::Time { precision: Some(0), tz: false });
        assert_eq!(p("SECONDDATE"), L::Timestamp { precision: Some(0), tz: false });
        assert_eq!(p("TIMESTAMP"), L::Timestamp { precision: Some(7), tz: false });
        assert_eq!(p("ST_POINT(4326)"), L::Geometry { kind: Some("point".into()), srid: Some(4326), geography: false });
        assert_eq!(p("ST_GEOMETRY"), L::Geometry { kind: None, srid: None, geography: false });
        assert!(matches!(p("REAL_VECTOR"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::Int { bytes: 1, unsigned: true }), "TINYINT");
        assert_eq!(r(L::int(1)), "SMALLINT");
        assert_eq!(r(L::Int { bytes: 2, unsigned: true }), "INTEGER");
        assert_eq!(r(L::int(8)), "BIGINT");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "DECIMAL(20, 0)");
        assert_eq!(r(L::int(16)), "DECIMAL(38, 0)");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "DECIMAL(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(50), scale: Some(2) }), "DECIMAL");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "DECIMAL");
        assert_eq!(r(L::Float { bytes: 4 }), "REAL");
        assert_eq!(r(L::Float { bytes: 8 }), "DOUBLE");
        assert_eq!(r(L::Money), "DECIMAL(19, 4)");
        assert_eq!(r(L::Char { len: Some(2), unicode: false }), "NCHAR(2)");
        assert_eq!(r(L::Char { len: Some(3000), unicode: false }), "NVARCHAR(3000)");
        assert_eq!(r(L::Varchar { len: Some(5000), unicode: true }), "NVARCHAR(5000)");
        assert_eq!(r(L::Varchar { len: Some(5001), unicode: true }), "NCLOB");
        assert_eq!(r(L::Text { unicode: false }), "NCLOB");
        assert_eq!(r(L::Binary { len: Some(16) }), "BINARY(16)");
        assert_eq!(r(L::Varbinary { len: Some(100) }), "VARBINARY(100)");
        assert_eq!(r(L::Blob), "BLOB");
        assert_eq!(r(L::Bit { len: Some(1) }), "BOOLEAN");
        assert_eq!(r(L::Bit { len: Some(9) }), "VARBINARY(2)");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: Some(6), tz: false }), "TIME");
        assert_eq!(r(L::Timestamp { precision: Some(0), tz: false }), "SECONDDATE");
        assert_eq!(r(L::Timestamp { precision: Some(6), tz: true }), "TIMESTAMP");
        assert!(Hana.render_type(&L::Timestamp { precision: Some(6), tz: true }).notes.iter().any(|n| n.code == IssueCode::TimeZoneLoss));
        assert!(Hana.render_type(&L::Timestamp { precision: Some(9), tz: false }).notes.iter().any(|n| n.code == IssueCode::PrecisionLoss));
        assert_eq!(r(L::Interval), "NVARCHAR(100)");
        assert_eq!(r(L::Year), "SMALLINT");
        assert_eq!(r(L::Uuid), "VARBINARY(16)");
        assert_eq!(r(L::Json { binary: false }), "NCLOB");
        assert_eq!(r(L::Xml), "NCLOB");
        assert_eq!(r(L::Enum { values: vec!["ab".into()] }), "NVARCHAR(2)");
        assert_eq!(r(L::Set { values: vec!["ab".into()] }), "NVARCHAR(2)");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }), "NCLOB");
        assert_eq!(r(L::Map { key: Box::new(L::int(4)), value: Box::new(L::int(4)) }), "NCLOB");
        assert_eq!(r(L::Geometry { kind: Some("point".into()), srid: Some(4326), geography: false }), "ST_POINT(4326)");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: true }), "ST_GEOMETRY(4326)");
        assert_eq!(r(L::Inet), "NVARCHAR(45)");
        assert_eq!(r(L::MacAddr), "NVARCHAR(17)");
        assert_eq!(r(L::RowVersion), "VARBINARY(8)");
        assert_eq!(r(L::Other { native: "REAL_VECTOR".into() }), "REAL_VECTOR");
    }

    #[test]
    fn round_trips_its_own_spellings() {
        for t in [
            L::Bool,
            L::Int { bytes: 1, unsigned: true },
            L::int(2),
            L::int(4),
            L::int(8),
            L::Decimal { precision: Some(12), scale: Some(2) },
            L::Decimal { precision: None, scale: None },
            L::Float { bytes: 8 },
            L::Varchar { len: Some(40), unicode: true },
            L::Char { len: Some(3), unicode: true },
            L::Text { unicode: true },
            L::Varbinary { len: Some(16) },
            L::Blob,
            L::Date,
            L::Timestamp { precision: Some(7), tz: false },
            L::Timestamp { precision: Some(0), tz: false },
        ] {
            let native = r(t.clone());
            assert_eq!(p(&native), t, "{native}");
        }
    }

    #[test]
    fn defaults() {
        let d = |v: DefaultValue, t: L| Hana.render_default(&v, &t);
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: None, tz: false }).as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(d(DefaultValue::CurrentDate, L::Date).as_deref(), Some("CURRENT_DATE"));
        assert_eq!(d(DefaultValue::CurrentTime, L::Time { precision: None, tz: false }).as_deref(), Some("CURRENT_TIME"));
        assert_eq!(d(DefaultValue::Bool(true), L::Bool).as_deref(), Some("TRUE"));
        assert_eq!(d(DefaultValue::NewUuid, L::Uuid), None);
        assert_eq!(d(DefaultValue::Text("x".into()), L::Text { unicode: true }), None);
    }
}
