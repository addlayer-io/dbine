//! IBM Informix and GBase 8s (an Informix derivative with the same type
//! system and catalog).
//!
//! The ODBC driver reports lower-case names: `integer`, `serial8`,
//! `decimal(16,2)`, `lvarchar(2048)`, `datetime year to fraction(3)`,
//! `interval day(3) to second`. A DATETIME is a range of fields: the
//! qualifier says whether it's a date, a time or a timestamp and how many
//! fraction digits (up to 5) it keeps.

use super::postgres::{longest, precision_loss};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{parse, TypeSpec};

pub struct Informix;

const MAX_VARCHAR: u32 = 255;
const MAX_LVARCHAR: u32 = 32739;
const MAX_CHAR: u32 = 32767;
const MAX_DECIMAL: u32 = 32;
const MAX_FRACTION: u8 = 5;

/// `YEAR TO FRACTION(p)` / `YEAR TO SECOND`.
fn datetime(prefix: &str, precision: Option<u8>) -> String {
    match precision {
        Some(0) => format!("{prefix} TO SECOND"),
        p => format!("{prefix} TO FRACTION({})", p.unwrap_or(MAX_FRACTION).min(MAX_FRACTION)),
    }
}

impl Dialect for Informix {
    fn id(&self) -> &'static str {
        "informix"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let frac = || Some(p(0).unwrap_or(3).min(MAX_FRACTION as u32) as u8);
        match t.name.as_str() {
            "boolean" => L::Bool,
            "smallint" => L::int(2),
            "integer" | "int" | "serial" => L::int(4),
            "bigint" | "int8" | "serial8" | "bigserial" => L::int(8),
            // DECIMAL(p) without scale is a floating decimal of p digits
            // (in non-ANSI databases); scale 255 says the same.
            "decimal" | "dec" | "numeric" => match (p(0), p(1)) {
                (Some(pr), Some(s)) if s < 255 => L::Decimal { precision: Some(pr), scale: Some(s) },
                _ => L::Decimal { precision: None, scale: None },
            },
            // MONEY(p, s) is a DECIMAL(p, s) shown with a currency symbol.
            "money" => match (p(0).unwrap_or(16), p(1).unwrap_or(2)) {
                (pr, s) if pr - s.min(pr) <= 15 && s <= 4 => L::Money,
                (pr, s) => L::Decimal { precision: Some(pr), scale: Some(s) },
            },
            "smallfloat" | "real" => L::Float { bytes: 4 },
            "float" | "double precision" => L::Float { bytes: 8 },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: false },
            "nchar" => L::Char { len: p(0).or(Some(1)), unicode: true },
            // VARCHAR(max, reserve).
            "varchar" | "character varying" => L::Varchar { len: p(0).or(Some(1)), unicode: false },
            "nvarchar" => L::Varchar { len: p(0).or(Some(1)), unicode: true },
            "lvarchar" => L::Varchar { len: p(0).or(Some(2048)), unicode: false },
            "text" | "clob" => L::Text { unicode: false },
            "byte" | "blob" => L::Blob,
            "date" => L::Date,
            "datetime year to day" => L::Date,
            "datetime year to second" | "datetime year to minute" | "datetime year to hour" => L::Timestamp { precision: Some(0), tz: false },
            "datetime year to fraction" => L::Timestamp { precision: frac(), tz: false },
            "datetime hour to second" | "datetime hour to minute" => L::Time { precision: Some(0), tz: false },
            "datetime hour to fraction" => L::Time { precision: frac(), tz: false },
            "datetime" => L::Timestamp { precision: None, tz: false },
            "json" => L::Json { binary: false },
            "bson" => L::Json { binary: true },
            // Collections: LIST(integer NOT NULL), SET(…), MULTISET(…).
            "list" | "set" | "multiset" if !t.args.is_empty() => {
                L::Array { of: Box::new(self.parse_type(&parse(&t.args[0]))) }
            }
            "st_geometry" | "st_point" | "st_linestring" | "st_polygon" | "st_multipoint" | "st_multilinestring" | "st_multipolygon" => {
                L::Geometry { kind: t.name.strip_prefix("st_").filter(|k| *k != "geometry").map(str::to_string), srid: None, geography: false }
            }
            n if n.starts_with("interval") => L::Interval,
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
                    _ if *bytes == 8 => Rendered::exact("DECIMAL(20, 0)"),
                    _ => Rendered::exact("DECIMAL(32, 0)").with(Loss, RangeLoss, "Entero de 16 bytes: Informix admite hasta 32 dígitos."),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, "Informix no tiene enteros sin signo: se usa un tipo más grande con signo.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } if *p <= MAX_DECIMAL => Rendered::exact(format!("DECIMAL({p}, {})", scale.unwrap_or(0))),
            L::Decimal { precision: Some(p), scale } => Rendered::exact(format!("DECIMAL({MAX_DECIMAL}, {})", scale.unwrap_or(0).min(MAX_DECIMAL)))
                .with(Loss, PrecisionLoss, format!("Informix admite hasta {MAX_DECIMAL} dígitos; el origen tiene {p}.")),
            // DECIMAL(p) without scale: floating decimal.
            L::Decimal { precision: None, .. } => Rendered::exact("DECIMAL(32)")
                .with(Loss, PrecisionLoss, "Número sin precisión fija como DECIMAL(32) flotante: hasta 32 dígitos significativos."),
            L::Float { bytes: 4 } => Rendered::exact("SMALLFLOAT"),
            L::Float { .. } => Rendered::exact("FLOAT"),
            L::Money => Rendered::exact("MONEY(19, 4)"),
            L::Char { len, unicode } => match len.unwrap_or(1) {
                n if n <= MAX_CHAR => Rendered::exact(format!("{}({n})", if *unicode { "NCHAR" } else { "CHAR" })),
                n => Rendered::exact("TEXT").with(Info, TypeChanged, format!("CHAR({n}) supera el máximo de Informix: se usa TEXT.")),
            },
            L::Varchar { len: Some(n), unicode } if *n <= MAX_VARCHAR => {
                Rendered::exact(format!("{}({n})", if *unicode { "NVARCHAR" } else { "VARCHAR" }))
            }
            L::Varchar { len: Some(n), .. } if *n <= MAX_LVARCHAR => Rendered::exact(format!("LVARCHAR({n})")),
            L::Varchar { len: Some(n), .. } => Rendered::exact("TEXT")
                .with(Info, TypeChanged, format!("VARCHAR({n}) supera los {MAX_LVARCHAR} de LVARCHAR: se usa TEXT.")),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact("TEXT"),
            L::Binary { .. } | L::Varbinary { .. } => Rendered::exact("BYTE").with(Info, TypeChanged, "Informix no tiene binarios con largo: se usa BYTE."),
            L::Blob => Rendered::exact("BYTE"),
            L::Bit { len: Some(1) } => Rendered::exact("BOOLEAN"),
            L::Bit { .. } => Rendered::exact("BYTE").with(Warning, TypeApproximated, "Informix no tiene cadenas de bits: se guardan como binario."),
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => {
                let r = Rendered::exact(datetime("DATETIME HOUR", precision.or(Some(0)))).with_loss(precision_loss(*precision, MAX_FRACTION));
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Informix no guarda la zona horaria.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact(datetime("DATETIME YEAR", *precision)).with_loss(precision_loss(*precision, MAX_FRACTION));
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Informix no guarda la zona horaria: conviene convertir los valores a UTC al copiarlos.")
                } else {
                    r
                }
            }
            L::Interval => Rendered::exact("INTERVAL DAY(9) TO FRACTION(5)")
                .with(Warning, TypeApproximated, "Intervalo de días a segundos: los años y meses no entran en el mismo intervalo de Informix."),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            L::Uuid => Rendered::exact("CHAR(36)").with(Info, TypeChanged, "Informix no tiene UUID: se guarda como texto de 36 caracteres."),
            L::Json { .. } => Rendered::exact("TEXT").with(Info, TypeChanged, "JSON como TEXT (los tipos JSON/BSON de Informix son para las colecciones NoSQL)."),
            L::Xml => Rendered::exact("TEXT").with(Info, TypeChanged, "Informix no tiene tipo XML: se guarda como TEXT."),
            L::Enum { values } | L::Set { values } => {
                let r = self.render_type(&L::Varchar { len: Some(longest(values) as u32), unicode: false });
                Rendered { native: r.native, notes: vec![] }
                    .with(Warning, TypeApproximated, format!("Informix no tiene enumerados: queda como texto. Valores: {}.", values.join(", ")))
            }
            L::Array { .. } | L::Map { .. } => Rendered::exact("TEXT")
                .with(Warning, TypeApproximated, "Arreglos y mapas se guardan como JSON en TEXT."),
            L::Geometry { kind, .. } => {
                let shape = match kind.as_deref() {
                    Some(k @ ("point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon")) => k.to_ascii_uppercase(),
                    _ => "GEOMETRY".into(),
                };
                Rendered::exact(format!("ST_{shape}")).with(Warning, TypeChanged, "Los tipos espaciales requieren el Spatial DataBlade de Informix registrado en la base.")
            }
            L::Inet => Rendered::exact("VARCHAR(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("BIGINT")
                .with(Warning, TypeApproximated, "La versión de fila queda como número y no se actualiza sola (en Informix se usa WITH VERCOLS)."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        // TEXT and BYTE only take DEFAULT NULL.
        let native = self.render_type(ty).native;
        if matches!(native.as_str(), "TEXT" | "BYTE") && !matches!(d, DefaultValue::Null) {
            return None;
        }
        // CURRENT must carry the column's own qualifier.
        let current = |ty: &L| match ty {
            L::Date => "TODAY".to_string(),
            L::Time { precision, .. } => format!("CURRENT {}", datetime("HOUR", precision.or(Some(0)))),
            L::Timestamp { precision, .. } => format!("CURRENT {}", datetime("YEAR", *precision)),
            _ => format!("CURRENT {}", datetime("YEAR", None)),
        };
        match d {
            DefaultValue::CurrentTimestamp => Some(current(ty)),
            DefaultValue::CurrentDate => Some("TODAY".into()),
            DefaultValue::CurrentTime => Some(current(match ty {
                L::Time { .. } => ty,
                _ => &L::Time { precision: Some(0), tz: false },
            })),
            DefaultValue::Bool(b) if matches!(ty, L::Bool | L::Bit { len: Some(1) }) => Some(if *b { "'t'" } else { "'f'" }.into()),
            DefaultValue::NewUuid => None,
            other => standard_default(other, ty, "CURRENT YEAR TO FRACTION(5)", None, true),
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            // Only ON DELETE CASCADE exists; without it Informix restricts,
            // which is what RESTRICT and NO ACTION ask for (the driver
            // writes no clause for them).
            on_delete: &["CASCADE", "RESTRICT", "NO ACTION"],
            on_update: &["RESTRICT", "NO ACTION"],
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: false,
            max_identifier: 128,
            case: IdentCase::Lower,
        }
    }

    fn implies_auto_increment(&self, t: &TypeSpec) -> bool {
        matches!(t.name.as_str(), "serial" | "serial8" | "bigserial")
    }
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Informix = Informix;
    matches!(driver_id, "informix" | "gbase8s").then_some(&D as &dyn Dialect)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> L {
        Informix.parse_type(&parse(s))
    }
    fn r(t: L) -> String {
        Informix.render_type(&t).native
    }

    #[test]
    fn parses_catalog_spellings() {
        assert_eq!(p("smallint"), L::int(2));
        assert_eq!(p("integer"), L::int(4));
        assert_eq!(p("serial"), L::int(4));
        assert_eq!(p("int8"), L::int(8));
        assert_eq!(p("bigint"), L::int(8));
        assert_eq!(p("serial8"), L::int(8));
        assert_eq!(p("bigserial"), L::int(8));
        assert!(Informix.implies_auto_increment(&parse("serial")));
        assert!(Informix.implies_auto_increment(&parse("bigserial")));
        assert!(!Informix.implies_auto_increment(&parse("integer")));
        assert_eq!(p("decimal(16,2)"), L::Decimal { precision: Some(16), scale: Some(2) });
        assert_eq!(p("decimal(16)"), L::Decimal { precision: None, scale: None });
        assert_eq!(p("decimal(16,255)"), L::Decimal { precision: None, scale: None });
        assert_eq!(p("money(16,2)"), L::Money);
        assert_eq!(p("money(30,2)"), L::Decimal { precision: Some(30), scale: Some(2) });
        assert_eq!(p("smallfloat"), L::Float { bytes: 4 });
        assert_eq!(p("float"), L::Float { bytes: 8 });
        assert_eq!(p("char(10)"), L::Char { len: Some(10), unicode: false });
        assert_eq!(p("nchar(10)"), L::Char { len: Some(10), unicode: true });
        assert_eq!(p("varchar(100,10)"), L::Varchar { len: Some(100), unicode: false });
        assert_eq!(p("nvarchar(100)"), L::Varchar { len: Some(100), unicode: true });
        assert_eq!(p("lvarchar(4000)"), L::Varchar { len: Some(4000), unicode: false });
        assert_eq!(p("lvarchar"), L::Varchar { len: Some(2048), unicode: false });
        assert_eq!(p("text"), L::Text { unicode: false });
        assert_eq!(p("clob"), L::Text { unicode: false });
        assert_eq!(p("byte"), L::Blob);
        assert_eq!(p("blob"), L::Blob);
        assert_eq!(p("boolean"), L::Bool);
        assert_eq!(p("date"), L::Date);
        assert_eq!(p("datetime year to day"), L::Date);
        assert_eq!(p("datetime year to second"), L::Timestamp { precision: Some(0), tz: false });
        assert_eq!(p("datetime year to fraction(3)"), L::Timestamp { precision: Some(3), tz: false });
        assert_eq!(p("DATETIME YEAR TO FRACTION(5)"), L::Timestamp { precision: Some(5), tz: false });
        assert_eq!(p("datetime year to fraction"), L::Timestamp { precision: Some(3), tz: false });
        assert_eq!(p("datetime hour to second"), L::Time { precision: Some(0), tz: false });
        assert_eq!(p("datetime hour to fraction(2)"), L::Time { precision: Some(2), tz: false });
        assert_eq!(p("interval day(3) to second"), L::Interval);
        assert_eq!(p("interval year to month"), L::Interval);
        assert_eq!(p("json"), L::Json { binary: false });
        assert_eq!(p("bson"), L::Json { binary: true });
        assert_eq!(p("list(integer not null)"), L::Array { of: Box::new(L::int(4)) });
        assert_eq!(p("st_point"), L::Geometry { kind: Some("point".into()), srid: None, geography: false });
        assert!(matches!(p("row(a int)"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(1)), "SMALLINT");
        assert_eq!(r(L::int(4)), "INTEGER");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "BIGINT");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "DECIMAL(20, 0)");
        assert_eq!(r(L::int(16)), "DECIMAL(32, 0)");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "DECIMAL(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(38), scale: Some(2) }), "DECIMAL(32, 2)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "DECIMAL(32)");
        assert_eq!(r(L::Float { bytes: 4 }), "SMALLFLOAT");
        assert_eq!(r(L::Float { bytes: 8 }), "FLOAT");
        assert_eq!(r(L::Money), "MONEY(19, 4)");
        assert_eq!(r(L::Char { len: Some(3), unicode: false }), "CHAR(3)");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }), "NCHAR(3)");
        assert_eq!(r(L::Varchar { len: Some(100), unicode: false }), "VARCHAR(100)");
        assert_eq!(r(L::Varchar { len: Some(100), unicode: true }), "NVARCHAR(100)");
        assert_eq!(r(L::Varchar { len: Some(1000), unicode: true }), "LVARCHAR(1000)");
        assert_eq!(r(L::Varchar { len: Some(40000), unicode: true }), "TEXT");
        assert_eq!(r(L::Text { unicode: true }), "TEXT");
        assert_eq!(r(L::Binary { len: Some(16) }), "BYTE");
        assert_eq!(r(L::Varbinary { len: Some(16) }), "BYTE");
        assert_eq!(r(L::Blob), "BYTE");
        assert_eq!(r(L::Bit { len: Some(1) }), "BOOLEAN");
        assert_eq!(r(L::Bit { len: Some(8) }), "BYTE");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: None, tz: false }), "DATETIME HOUR TO SECOND");
        assert_eq!(r(L::Time { precision: Some(3), tz: false }), "DATETIME HOUR TO FRACTION(3)");
        assert_eq!(r(L::Timestamp { precision: Some(0), tz: false }), "DATETIME YEAR TO SECOND");
        assert_eq!(r(L::Timestamp { precision: Some(3), tz: false }), "DATETIME YEAR TO FRACTION(3)");
        assert_eq!(r(L::Timestamp { precision: None, tz: false }), "DATETIME YEAR TO FRACTION(5)");
        assert_eq!(r(L::Timestamp { precision: Some(7), tz: true }), "DATETIME YEAR TO FRACTION(5)");
        assert_eq!(Informix.render_type(&L::Timestamp { precision: Some(7), tz: true }).notes.len(), 2);
        assert_eq!(r(L::Interval), "INTERVAL DAY(9) TO FRACTION(5)");
        assert_eq!(r(L::Year), "SMALLINT");
        assert_eq!(r(L::Uuid), "CHAR(36)");
        assert_eq!(r(L::Json { binary: true }), "TEXT");
        assert_eq!(r(L::Xml), "TEXT");
        assert_eq!(r(L::Enum { values: vec!["abc".into()] }), "VARCHAR(3)");
        assert_eq!(r(L::Set { values: vec!["abc".into()] }), "VARCHAR(3)");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }), "TEXT");
        assert_eq!(r(L::Map { key: Box::new(L::int(4)), value: Box::new(L::int(4)) }), "TEXT");
        assert_eq!(r(L::Geometry { kind: Some("polygon".into()), srid: None, geography: false }), "ST_POLYGON");
        assert_eq!(r(L::Inet), "VARCHAR(45)");
        assert_eq!(r(L::MacAddr), "VARCHAR(17)");
        assert_eq!(r(L::RowVersion), "BIGINT");
        assert_eq!(r(L::Other { native: "row(a int)".into() }), "row(a int)");
    }

    #[test]
    fn round_trips_its_own_spellings() {
        for t in [
            L::Bool,
            L::int(2),
            L::int(4),
            L::int(8),
            L::Decimal { precision: Some(16), scale: Some(2) },
            L::Money,
            L::Float { bytes: 4 },
            L::Char { len: Some(3), unicode: true },
            L::Varchar { len: Some(40), unicode: false },
            L::Varchar { len: Some(4000), unicode: false },
            L::Text { unicode: false },
            L::Blob,
            L::Date,
            L::Time { precision: Some(0), tz: false },
            L::Timestamp { precision: Some(3), tz: false },
            L::Timestamp { precision: Some(0), tz: false },
            L::Interval,
        ] {
            let native = r(t.clone()).to_ascii_lowercase();
            assert_eq!(p(&native), t, "{native}");
        }
    }

    #[test]
    fn defaults() {
        let d = |v: DefaultValue, t: L| Informix.render_default(&v, &t);
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: Some(0), tz: false }).as_deref(), Some("CURRENT YEAR TO SECOND"));
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: Some(3), tz: false }).as_deref(), Some("CURRENT YEAR TO FRACTION(3)"));
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: None, tz: true }).as_deref(), Some("CURRENT YEAR TO FRACTION(5)"));
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Date).as_deref(), Some("TODAY"));
        assert_eq!(d(DefaultValue::CurrentDate, L::Date).as_deref(), Some("TODAY"));
        assert_eq!(d(DefaultValue::CurrentTime, L::Time { precision: None, tz: false }).as_deref(), Some("CURRENT HOUR TO SECOND"));
        assert_eq!(d(DefaultValue::Bool(true), L::Bool).as_deref(), Some("'t'"));
        assert_eq!(d(DefaultValue::Bool(false), L::int(2)).as_deref(), Some("0"));
        assert_eq!(d(DefaultValue::Text("x".into()), L::Text { unicode: true }), None);
        assert_eq!(d(DefaultValue::Null, L::Text { unicode: true }).as_deref(), Some("NULL"));
        assert_eq!(d(DefaultValue::NewUuid, L::Uuid), None);
    }
}
