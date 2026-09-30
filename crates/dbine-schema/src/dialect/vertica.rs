//! OpenText (Micro Focus) Vertica.
//!
//! Every integer is 8 bytes and every float is a double. Text is UTF-8
//! and its lengths are **bytes**, so a column of n characters from an
//! engine that counts characters gets 4n bytes (up to 65000; beyond that,
//! LONG VARCHAR up to 32 MB). There are no user indexes (projections do
//! that job) and foreign keys aren't enforced.

use super::postgres::{longest, precision_loss, prec};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{parse, TypeSpec};

pub struct Vertica;

const MAX_BYTES: u32 = 65000;
const MAX_LONG: u32 = 32_000_000;
const MAX_DECIMAL: u32 = 1024;
const MAX_FRACTION: u8 = 6;
/// Bytes per character reserved for text measured in characters.
const UTF8_BYTES: u32 = 4;

impl Vertica {
    /// Text of `len` characters (or bytes, when not Unicode) in bytes.
    fn text(&self, len: Option<u32>, unicode: bool, fixed: bool) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let Some(n) = len else {
            return Rendered::exact(format!("LONG VARCHAR({MAX_LONG})"))
                .with(Warning, LengthLoss, "Texto sin límite como LONG VARCHAR: Vertica guarda hasta 32 MB por valor.");
        };
        let bytes = if unicode { n.saturating_mul(UTF8_BYTES) } else { n };
        let widened = |r: Rendered| {
            if unicode && bytes != n {
                r.with(Info, TypeChanged, format!("Vertica mide el texto en bytes: {n} caracteres pasan a {bytes} bytes (UTF-8)."))
            } else {
                r
            }
        };
        match bytes {
            b if b <= MAX_BYTES => widened(Rendered::exact(format!("{}({b})", if fixed { "CHAR" } else { "VARCHAR" }))),
            b if b <= MAX_LONG => widened(Rendered::exact(format!("LONG VARCHAR({b})"))),
            _ => Rendered::exact(format!("LONG VARCHAR({MAX_LONG})")).with(Loss, LengthLoss, format!("{n} caracteres superan los 32 MB de LONG VARCHAR.")),
        }
    }
}

impl Dialect for Vertica {
    fn id(&self) -> &'static str {
        "vertica"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let raw = t.raw.to_ascii_lowercase();
        // Native collections: ARRAY[INT], SET[VARCHAR(10)].
        for prefix in ["array[", "set["] {
            if let Some(inner) = raw.strip_prefix(prefix).and_then(|r| r.strip_suffix(']')) {
                let inner_raw = &t.raw[prefix.len()..prefix.len() + inner.len()];
                return L::Array { of: Box::new(self.parse_type(&parse(inner_raw))) };
            }
        }
        match t.name.as_str() {
            "boolean" | "bool" => L::Bool,
            // All Vertica integers are 64-bit.
            "int" | "integer" | "bigint" | "smallint" | "tinyint" | "int8" => L::int(8),
            "numeric" | "decimal" | "number" => L::Decimal { precision: p(0).or(Some(37)), scale: p(1).or(if p(0).is_some() { Some(0) } else { Some(15) }) },
            "money" => L::Money,
            "float" | "float8" | "real" | "double precision" | "double" => L::Float { bytes: 8 },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "character varying" => L::Varchar { len: p(0).or(Some(80)), unicode: true },
            "long varchar" => L::Varchar { len: p(0).or(Some(1_048_576)), unicode: true },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" | "binary varying" | "bytea" | "raw" => L::Varbinary { len: p(0).or(Some(80)) },
            "long varbinary" => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: Some(p(0).unwrap_or(6) as u8), tz: t.with_tz },
            "timetz" => L::Time { precision: Some(p(0).unwrap_or(6) as u8), tz: true },
            "timestamp" | "datetime" | "smalldatetime" => L::Timestamp { precision: Some(p(0).unwrap_or(6) as u8), tz: t.with_tz },
            "timestamptz" => L::Timestamp { precision: Some(p(0).unwrap_or(6) as u8), tz: true },
            "uuid" => L::Uuid,
            // The argument is the maximum size in bytes, not an SRID.
            "geometry" => L::Geometry { kind: None, srid: None, geography: false },
            "geography" => L::Geometry { kind: None, srid: None, geography: true },
            n if n.starts_with("interval") => L::Interval,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => match L::signed_bytes_for(*bytes, *unsigned) {
                1..=8 => Rendered::exact("INTEGER"),
                _ if *bytes == 8 => Rendered::exact("NUMERIC(20, 0)").with(Info, TypeChanged, "Entero de 8 bytes sin signo como NUMERIC(20, 0)."),
                _ => Rendered::exact("NUMERIC(39, 0)").with(Info, TypeChanged, "Entero de 16 bytes como NUMERIC(39, 0)."),
            },
            L::Decimal { precision: Some(p), scale } if *p <= MAX_DECIMAL => Rendered::exact(format!("NUMERIC({p}, {})", scale.unwrap_or(0))),
            L::Decimal { precision: Some(p), scale } => Rendered::exact(format!("NUMERIC({MAX_DECIMAL}, {})", scale.unwrap_or(0).min(MAX_DECIMAL)))
                .with(Loss, PrecisionLoss, format!("Vertica admite hasta {MAX_DECIMAL} dígitos; el origen tiene {p}.")),
            L::Decimal { precision: None, .. } => Rendered::exact("NUMERIC(37, 15)")
                .with(Loss, PrecisionLoss, "El origen no fija la precisión: se usa NUMERIC(37, 15), el predeterminado de Vertica."),
            L::Float { .. } => Rendered::exact("FLOAT"),
            L::Money => Rendered::exact("MONEY"),
            L::Char { len, unicode } => self.text(len.or(Some(1)), *unicode, true),
            L::Varchar { len, unicode } => self.text(*len, *unicode, false),
            L::Text { .. } => self.text(None, true, false),
            L::Binary { len } => match len.unwrap_or(1) {
                n if n <= MAX_BYTES => Rendered::exact(format!("BINARY({n})")),
                n if n <= MAX_LONG => Rendered::exact(format!("LONG VARBINARY({n})")),
                _ => Rendered::exact(format!("LONG VARBINARY({MAX_LONG})")).with(Loss, LengthLoss, "Vertica guarda hasta 32 MB por valor binario."),
            },
            L::Varbinary { len: Some(n) } if *n <= MAX_BYTES => Rendered::exact(format!("VARBINARY({n})")),
            L::Varbinary { len: Some(n) } if *n <= MAX_LONG => Rendered::exact(format!("LONG VARBINARY({n})")),
            L::Varbinary { .. } | L::Blob => Rendered::exact(format!("LONG VARBINARY({MAX_LONG})"))
                .with(Warning, LengthLoss, "Binario sin límite como LONG VARBINARY: Vertica guarda hasta 32 MB por valor."),
            L::Bit { len: Some(1) } => Rendered::exact("BOOLEAN"),
            L::Bit { len } => self.render_type(&L::Varbinary { len: Some(len.map_or(MAX_BYTES, |n| n.div_ceil(8))) })
                .with(Warning, TypeApproximated, "Vertica no tiene cadenas de bits: se guardan como binario."),
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => Rendered::exact(format!("{}{}", if *tz { "TIMETZ" } else { "TIME" }, prec(*precision, MAX_FRACTION)))
                .with_loss(precision_loss(*precision, MAX_FRACTION)),
            L::Timestamp { precision, tz } => {
                Rendered::exact(format!("{}{}", if *tz { "TIMESTAMPTZ" } else { "TIMESTAMP" }, prec(*precision, MAX_FRACTION)))
                    .with_loss(precision_loss(*precision, MAX_FRACTION))
            }
            L::Interval => Rendered::exact("INTERVAL DAY TO SECOND")
                .with(Warning, TypeApproximated, "Intervalo de días a segundos: los años y meses van en otro tipo de intervalo en Vertica."),
            L::Year => Rendered::exact("INTEGER").with(Info, TypeChanged, "Año como INTEGER."),
            L::Uuid => Rendered::exact("UUID"),
            L::Json { .. } => Rendered::exact(format!("LONG VARCHAR({MAX_LONG})"))
                .with(Info, TypeChanged, "Vertica no tiene columnas JSON: se guarda como LONG VARCHAR (MAPJSONEXTRACTOR y las tablas flex lo leen)."),
            L::Xml => Rendered::exact(format!("LONG VARCHAR({MAX_LONG})")).with(Info, TypeChanged, "Vertica no tiene tipo XML: se guarda como LONG VARCHAR."),
            L::Enum { values } | L::Set { values } => {
                let r = self.text(Some(longest(values) as u32), true, false);
                Rendered { native: r.native, notes: vec![] }
                    .with(Warning, TypeApproximated, format!("Vertica no tiene enumerados: queda como texto. Valores: {}.", values.join(", ")))
            }
            L::Array { of } => {
                let inner = self.render_type(of);
                let scalar = !matches!(**of, L::Array { .. } | L::Map { .. } | L::Json { .. } | L::Xml | L::Other { .. } | L::Geometry { .. })
                    && !inner.native.starts_with("LONG ");
                if scalar {
                    Rendered { native: format!("ARRAY[{}]", inner.native), notes: inner.notes }
                } else {
                    Rendered::exact(format!("LONG VARCHAR({MAX_LONG})")).with(Warning, TypeApproximated, "Arreglo de un tipo que Vertica no admite en colecciones: se guarda como JSON en texto.")
                }
            }
            L::Map { .. } => Rendered::exact(format!("LONG VARCHAR({MAX_LONG})")).with(Warning, TypeApproximated, "Vertica no tiene mapas: se guardan como JSON en texto."),
            L::Geometry { srid, geography, .. } => {
                let r = Rendered::exact(if *geography { "GEOGRAPHY" } else { "GEOMETRY" });
                if srid.is_some() {
                    r.with(Info, TypeChanged, "El SRID no es parte del tipo en Vertica: va en cada valor.")
                } else {
                    r
                }
            }
            L::Inet => Rendered::exact("VARCHAR(45)").with(Info, TypeApproximated, "Dirección IP como texto (INET_ATON la convierte a número)."),
            L::MacAddr => Rendered::exact("VARCHAR(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("VARBINARY(8)")
                .with(Warning, TypeApproximated, "Vertica no tiene versión de fila automática: queda como binario y no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp => Some(match ty {
                L::Date => "CURRENT_DATE".into(),
                L::Time { tz: true, .. } => "CURRENT_TIME".into(),
                L::Time { .. } => "LOCALTIME".into(),
                L::Timestamp { tz: true, .. } => "CURRENT_TIMESTAMP".into(),
                _ => "LOCALTIMESTAMP".into(),
            }),
            DefaultValue::CurrentTime => Some(if matches!(ty, L::Time { tz: true, .. }) { "CURRENT_TIME" } else { "LOCALTIME" }.into()),
            other => standard_default(other, ty, "LOCALTIMESTAMP", Some("UUID_GENERATE()"), false),
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            // Declared, not enforced; no referential actions.
            foreign_keys: true,
            on_delete: &[],
            on_update: &[],
            indexes: false,
            partial_indexes: false,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: false,
            max_identifier: 128,
            case: IdentCase::Preserve,
        }
    }

    fn implies_auto_increment(&self, t: &TypeSpec) -> bool {
        matches!(t.name.as_str(), "identity" | "auto_increment")
    }
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Vertica = Vertica;
    (driver_id == "vertica").then_some(&D as &dyn Dialect)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> L {
        Vertica.parse_type(&parse(s))
    }
    fn r(t: L) -> String {
        Vertica.render_type(&t).native
    }

    #[test]
    fn parses_catalog_spellings() {
        for i in ["int", "Integer", "bigint", "smallint", "tinyint", "int8"] {
            assert_eq!(p(i), L::int(8), "{i}");
        }
        assert_eq!(p("numeric(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(p("numeric"), L::Decimal { precision: Some(37), scale: Some(15) });
        assert_eq!(p("money"), L::Money);
        assert_eq!(p("float"), L::Float { bytes: 8 });
        assert_eq!(p("real"), L::Float { bytes: 8 });
        assert_eq!(p("boolean"), L::Bool);
        assert_eq!(p("char(10)"), L::Char { len: Some(10), unicode: true });
        assert_eq!(p("varchar(80)"), L::Varchar { len: Some(80), unicode: true });
        assert_eq!(p("varchar"), L::Varchar { len: Some(80), unicode: true });
        assert_eq!(p("long varchar(1048576)"), L::Varchar { len: Some(1_048_576), unicode: true });
        assert_eq!(p("binary(16)"), L::Binary { len: Some(16) });
        assert_eq!(p("varbinary(80)"), L::Varbinary { len: Some(80) });
        assert_eq!(p("long varbinary(1048576)"), L::Blob);
        assert_eq!(p("date"), L::Date);
        assert_eq!(p("time"), L::Time { precision: Some(6), tz: false });
        assert_eq!(p("timetz(3)"), L::Time { precision: Some(3), tz: true });
        assert_eq!(p("timestamp"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(p("timestamptz"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(p("timestamp(3) with time zone"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(p("interval day to second"), L::Interval);
        assert_eq!(p("interval year to month"), L::Interval);
        assert_eq!(p("uuid"), L::Uuid);
        assert_eq!(p("geometry(1048576)"), L::Geometry { kind: None, srid: None, geography: false });
        assert_eq!(p("geography"), L::Geometry { kind: None, srid: None, geography: true });
        assert_eq!(p("Array[int]"), L::Array { of: Box::new(L::int(8)) });
        assert_eq!(p("array[varchar(10)]"), L::Array { of: Box::new(L::Varchar { len: Some(10), unicode: true }) });
        assert!(matches!(p("row(a int)"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(2)), "INTEGER");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "INTEGER");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "NUMERIC(20, 0)");
        assert_eq!(r(L::int(16)), "NUMERIC(39, 0)");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "NUMERIC(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(60), scale: Some(2) }), "NUMERIC(60, 2)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "NUMERIC(37, 15)");
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Money), "MONEY");
        assert_eq!(r(L::Char { len: Some(3), unicode: false }), "CHAR(3)");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }), "CHAR(12)");
        assert_eq!(r(L::Varchar { len: Some(20), unicode: true }), "VARCHAR(80)");
        assert_eq!(r(L::Varchar { len: Some(20), unicode: false }), "VARCHAR(20)");
        assert_eq!(r(L::Varchar { len: Some(20000), unicode: true }), "LONG VARCHAR(80000)");
        assert_eq!(r(L::Text { unicode: true }), "LONG VARCHAR(32000000)");
        assert_eq!(r(L::Binary { len: Some(16) }), "BINARY(16)");
        assert_eq!(r(L::Varbinary { len: Some(100) }), "VARBINARY(100)");
        assert_eq!(r(L::Blob), "LONG VARBINARY(32000000)");
        assert_eq!(r(L::Bit { len: Some(1) }), "BOOLEAN");
        assert_eq!(r(L::Bit { len: Some(9) }), "VARBINARY(2)");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: Some(3), tz: true }), "TIMETZ(3)");
        assert_eq!(r(L::Timestamp { precision: None, tz: false }), "TIMESTAMP");
        assert_eq!(r(L::Timestamp { precision: Some(7), tz: true }), "TIMESTAMPTZ(6)");
        assert_eq!(r(L::Interval), "INTERVAL DAY TO SECOND");
        assert_eq!(r(L::Year), "INTEGER");
        assert_eq!(r(L::Uuid), "UUID");
        assert_eq!(r(L::Json { binary: true }), "LONG VARCHAR(32000000)");
        assert_eq!(r(L::Xml), "LONG VARCHAR(32000000)");
        assert_eq!(r(L::Enum { values: vec!["abc".into()] }), "VARCHAR(12)");
        assert_eq!(r(L::Set { values: vec!["abc".into()] }), "VARCHAR(12)");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }), "ARRAY[INTEGER]");
        assert_eq!(r(L::Array { of: Box::new(L::Text { unicode: true }) }), "LONG VARCHAR(32000000)");
        assert_eq!(r(L::Map { key: Box::new(L::int(4)), value: Box::new(L::int(4)) }), "LONG VARCHAR(32000000)");
        assert_eq!(r(L::Geometry { kind: None, srid: Some(4326), geography: true }), "GEOGRAPHY");
        assert_eq!(r(L::Inet), "VARCHAR(45)");
        assert_eq!(r(L::MacAddr), "VARCHAR(17)");
        assert_eq!(r(L::RowVersion), "VARBINARY(8)");
        assert_eq!(r(L::Other { native: "row(a int)".into() }), "row(a int)");
    }

    #[test]
    fn defaults() {
        let d = |v: DefaultValue, t: L| Vertica.render_default(&v, &t);
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: None, tz: false }).as_deref(), Some("LOCALTIMESTAMP"));
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: None, tz: true }).as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(d(DefaultValue::CurrentDate, L::Date).as_deref(), Some("CURRENT_DATE"));
        assert_eq!(d(DefaultValue::NewUuid, L::Uuid).as_deref(), Some("UUID_GENERATE()"));
        assert_eq!(d(DefaultValue::Bool(true), L::Bool).as_deref(), Some("TRUE"));
    }
}
