//! Teradata Vantage.
//!
//! Text columns carry a server character set: LATIN (one byte per
//! character, the usual default) or UNICODE (UTF-16), written after the
//! length: `VARCHAR(100) CHARACTER SET UNICODE`. All floats are 8 bytes;
//! BYTEINT is the 1-byte signed integer; NUMBER without precision is a
//! floating decimal like Oracle's. There's no BOOLEAN.

use super::postgres::{longest, precision_loss, prec};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;

pub struct Teradata;

const MAX_LATIN: u32 = 64000;
const MAX_UNICODE: u32 = 32000;
const MAX_BYTES: u32 = 64000;
const MAX_DECIMAL: u32 = 38;
const MAX_FRACTION: u8 = 6;
const UNICODE: &str = " CHARACTER SET UNICODE";

impl Teradata {
    fn text(&self, len: Option<u32>, unicode: bool, fixed: bool) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let (max, cs) = if unicode { (MAX_UNICODE, UNICODE) } else { (MAX_LATIN, "") };
        let kind = if fixed { "CHAR" } else { "VARCHAR" };
        match len {
            Some(n) if n <= max => Rendered::exact(format!("{kind}({n}){cs}")),
            Some(n) => Rendered::exact(format!("CLOB{cs}")).with(Info, TypeChanged, format!("{kind}({n}) supera el máximo de {max} de Teradata: se usa CLOB.")),
            None if fixed => Rendered::exact(format!("CHAR(1){cs}")),
            None => Rendered::exact(format!("CLOB{cs}")),
        }
    }
}

impl Dialect for Teradata {
    fn id(&self) -> &'static str {
        "teradata"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        // `CLOB CHARACTER SET UNICODE` keeps the clause in the name;
        // `VARCHAR(10) CHARACTER SET UNICODE` leaves it in `rest`.
        let (name, cs_in_name) = match t.name.split_once(" character set ") {
            Some((n, cs)) => (n, Some(cs)),
            None => (t.name.as_str(), None),
        };
        let unicode = cs_in_name.is_some_and(|cs| cs.starts_with("unicode") || cs.starts_with("graphic"))
            || t.has("unicode")
            || t.has("graphic");
        let frac = || Some(p(0).unwrap_or(6).min(MAX_FRACTION as u32) as u8);
        match name {
            "byteint" => L::int(1),
            "smallint" => L::int(2),
            "integer" | "int" => L::int(4),
            "bigint" => L::int(8),
            "decimal" | "numeric" | "dec" => L::Decimal { precision: p(0).or(Some(5)), scale: p(1).or(Some(0)) },
            // NUMBER / NUMBER(*): floating decimal, up to 38 digits.
            "number" => match t.args.first().map(|a| a.trim()) {
                Some(a) if a != "*" => L::Decimal { precision: p(0), scale: p(1).or(Some(0)) },
                _ => L::Decimal { precision: None, scale: None },
            },
            "float" | "real" | "double precision" => L::Float { bytes: 8 },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode },
            "varchar" | "character varying" | "char varying" => L::Varchar { len: p(0), unicode },
            "long varchar" => L::Varchar { len: Some(if unicode { MAX_UNICODE } else { MAX_LATIN }), unicode },
            "graphic" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "vargraphic" => L::Varchar { len: p(0), unicode: true },
            "long vargraphic" => L::Varchar { len: Some(MAX_UNICODE), unicode: true },
            "clob" | "character large object" => L::Text { unicode },
            "byte" => L::Binary { len: p(0).or(Some(1)) },
            "varbyte" => L::Varbinary { len: p(0) },
            "blob" | "binary large object" => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: frac(), tz: t.with_tz },
            "timestamp" => L::Timestamp { precision: frac(), tz: t.with_tz },
            "json" => L::Json { binary: t.has("bson") || t.has("ubjson") },
            "xml" => L::Xml,
            "st_geometry" | "sysudtlib.st_geometry" => L::Geometry { kind: None, srid: None, geography: false },
            n if n.starts_with("interval") => L::Interval,
            // PERIOD(…), DATASET, MBR, UDTs.
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("BYTEINT").with(Info, TypeChanged, "Teradata no tiene BOOLEAN: se usa BYTEINT (0/1)."),
            L::Int { bytes: 1, unsigned: false } => Rendered::exact("BYTEINT"),
            L::Int { bytes, unsigned } => {
                let r = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 | 2 => Rendered::exact("SMALLINT"),
                    3 | 4 => Rendered::exact("INTEGER"),
                    8 => Rendered::exact("BIGINT"),
                    _ if *bytes == 8 => Rendered::exact("DECIMAL(20, 0)"),
                    _ => Rendered::exact("DECIMAL(38, 0)").with(Loss, RangeLoss, "Entero de 16 bytes como DECIMAL(38, 0): no entran los valores de 39 dígitos."),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, "Teradata no tiene enteros sin signo: se usa un tipo más grande con signo.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } if *p <= MAX_DECIMAL => Rendered::exact(format!("DECIMAL({p}, {})", scale.unwrap_or(0))),
            L::Decimal { precision: Some(p), scale } => Rendered::exact(format!("DECIMAL(38, {})", scale.unwrap_or(0).min(38)))
                .with(Loss, PrecisionLoss, format!("Teradata admite hasta 38 dígitos; el origen tiene {p}.")),
            L::Decimal { precision: None, .. } => Rendered::exact("NUMBER"),
            L::Float { .. } => Rendered::exact("FLOAT"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(Info, TypeChanged, "Moneda como DECIMAL(19, 4)."),
            L::Char { len, unicode } => self.text(len.or(Some(1)), *unicode, true),
            L::Varchar { len, unicode } => self.text(*len, *unicode, false),
            L::Text { unicode } => Rendered::exact(format!("CLOB{}", if *unicode { UNICODE } else { "" })),
            L::Binary { len } => match len.unwrap_or(1) {
                n if n <= MAX_BYTES => Rendered::exact(format!("BYTE({n})")),
                _ => Rendered::exact("BLOB"),
            },
            L::Varbinary { len: Some(n) } if *n <= MAX_BYTES => Rendered::exact(format!("VARBYTE({n})")),
            L::Varbinary { .. } | L::Blob => Rendered::exact("BLOB"),
            L::Bit { len: Some(1) } => self.render_type(&L::Bool),
            L::Bit { len } => self.render_type(&L::Varbinary { len: len.map(|n| n.div_ceil(8)) })
                .with(Warning, TypeApproximated, "Teradata no tiene cadenas de bits: se guardan como binario."),
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => Rendered::exact(format!("TIME{}{}", prec(*precision, MAX_FRACTION), if *tz { " WITH TIME ZONE" } else { "" }))
                .with_loss(precision_loss(*precision, MAX_FRACTION)),
            L::Timestamp { precision, tz } => {
                Rendered::exact(format!("TIMESTAMP{}{}", prec(*precision, MAX_FRACTION), if *tz { " WITH TIME ZONE" } else { "" }))
                    .with_loss(precision_loss(*precision, MAX_FRACTION))
            }
            L::Interval => Rendered::exact("INTERVAL DAY(4) TO SECOND(6)")
                .with(Warning, TypeApproximated, "Intervalo de días a segundos (hasta 9999 días): los años y meses no entran en el mismo intervalo de Teradata."),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            L::Uuid => Rendered::exact("CHAR(36)").with(Info, TypeChanged, "Teradata no tiene UUID: se guarda como texto de 36 caracteres."),
            L::Json { .. } => Rendered::exact(format!("JSON(8388096){UNICODE}")),
            L::Xml => Rendered::exact("XML"),
            L::Enum { values } | L::Set { values } => {
                let r = self.text(Some(longest(values) as u32), true, false);
                Rendered { native: r.native, notes: vec![] }
                    .with(Warning, TypeApproximated, format!("Teradata no tiene enumerados: queda como texto. Valores: {}.", values.join(", ")))
            }
            L::Array { .. } | L::Map { .. } => Rendered::exact(format!("JSON(8388096){UNICODE}"))
                .with(Warning, TypeApproximated, "Arreglos y mapas se guardan como JSON."),
            L::Geometry { .. } => Rendered::exact("ST_GEOMETRY"),
            L::Inet => Rendered::exact("VARCHAR(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("BYTE(8)")
                .with(Warning, TypeApproximated, "Teradata no tiene versión de fila automática: queda como binario y no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        // CURRENT_TIMESTAMP must match the column's fractional digits.
        let with_prec = |f: &str, p: &Option<u8>| match p {
            Some(p) if *p != 6 => format!("{f}({})", p.min(&MAX_FRACTION)),
            _ => f.to_string(),
        };
        match d {
            DefaultValue::CurrentTimestamp => Some(match ty {
                L::Date => "CURRENT_DATE".into(),
                L::Time { precision, .. } => with_prec("CURRENT_TIME", precision),
                L::Timestamp { precision, .. } => with_prec("CURRENT_TIMESTAMP", precision),
                _ => "CURRENT_TIMESTAMP".into(),
            }),
            DefaultValue::CurrentTime => Some(match ty {
                L::Time { precision, .. } => with_prec("CURRENT_TIME", precision),
                _ => "CURRENT_TIME".into(),
            }),
            DefaultValue::NewUuid => None,
            DefaultValue::Text(_) if matches!(ty, L::Text { .. } | L::Json { .. } | L::Xml | L::Blob) => None,
            other => standard_default(other, ty, "CURRENT_TIMESTAMP", None, true),
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            // References restrict; there are no referential actions.
            on_delete: &[],
            on_update: &[],
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 128,
            case: IdentCase::Preserve,
        }
    }
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Teradata = Teradata;
    (driver_id == "teradata").then_some(&D as &dyn Dialect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn p(s: &str) -> L {
        Teradata.parse_type(&parse(s))
    }
    fn r(t: L) -> String {
        Teradata.render_type(&t).native
    }

    #[test]
    fn parses_catalog_spellings() {
        assert_eq!(p("BYTEINT"), L::int(1));
        assert_eq!(p("SMALLINT"), L::int(2));
        assert_eq!(p("INTEGER"), L::int(4));
        assert_eq!(p("BIGINT"), L::int(8));
        assert_eq!(p("DECIMAL(18,2)"), L::Decimal { precision: Some(18), scale: Some(2) });
        assert_eq!(p("NUMBER"), L::Decimal { precision: None, scale: None });
        assert_eq!(p("NUMBER(*)"), L::Decimal { precision: None, scale: None });
        assert_eq!(p("NUMBER(10,2)"), L::Decimal { precision: Some(10), scale: Some(2) });
        assert_eq!(p("FLOAT"), L::Float { bytes: 8 });
        assert_eq!(p("REAL"), L::Float { bytes: 8 });
        assert_eq!(p("CHAR(10)"), L::Char { len: Some(10), unicode: false });
        assert_eq!(p("VARCHAR(255)"), L::Varchar { len: Some(255), unicode: false });
        assert_eq!(p("VARCHAR(255) CHARACTER SET UNICODE"), L::Varchar { len: Some(255), unicode: true });
        assert_eq!(p("VARCHAR(255) CHARACTER SET LATIN NOT CASESPECIFIC"), L::Varchar { len: Some(255), unicode: false });
        assert_eq!(p("LONG VARCHAR"), L::Varchar { len: Some(64000), unicode: false });
        assert_eq!(p("CLOB"), L::Text { unicode: false });
        assert_eq!(p("CLOB CHARACTER SET UNICODE"), L::Text { unicode: true });
        assert_eq!(p("VARGRAPHIC(10)"), L::Varchar { len: Some(10), unicode: true });
        assert_eq!(p("BYTE(16)"), L::Binary { len: Some(16) });
        assert_eq!(p("VARBYTE(255)"), L::Varbinary { len: Some(255) });
        assert_eq!(p("BLOB"), L::Blob);
        assert_eq!(p("DATE"), L::Date);
        assert_eq!(p("TIME"), L::Time { precision: Some(6), tz: false });
        assert_eq!(p("TIME(0) WITH TIME ZONE"), L::Time { precision: Some(0), tz: true });
        assert_eq!(p("TIMESTAMP(6)"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(p("TIMESTAMP(3) WITH TIME ZONE"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(p("INTERVAL DAY(4) TO SECOND(6)"), L::Interval);
        assert_eq!(p("INTERVAL YEAR"), L::Interval);
        assert_eq!(p("JSON"), L::Json { binary: false });
        assert_eq!(p("JSON(1000) STORAGE FORMAT BSON"), L::Json { binary: true });
        assert_eq!(p("XML"), L::Xml);
        assert_eq!(p("ST_GEOMETRY"), L::Geometry { kind: None, srid: None, geography: false });
        assert!(matches!(p("PERIOD(DATE)"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        assert_eq!(r(L::Bool), "BYTEINT");
        assert_eq!(r(L::int(1)), "BYTEINT");
        assert_eq!(r(L::Int { bytes: 1, unsigned: true }), "SMALLINT");
        assert_eq!(r(L::int(4)), "INTEGER");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "BIGINT");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "DECIMAL(20, 0)");
        assert_eq!(r(L::int(16)), "DECIMAL(38, 0)");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "DECIMAL(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(40), scale: Some(2) }), "DECIMAL(38, 2)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "NUMBER");
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Money), "DECIMAL(19, 4)");
        assert_eq!(r(L::Char { len: Some(3), unicode: false }), "CHAR(3)");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }), "CHAR(3) CHARACTER SET UNICODE");
        assert_eq!(r(L::Varchar { len: Some(40), unicode: true }), "VARCHAR(40) CHARACTER SET UNICODE");
        assert_eq!(r(L::Varchar { len: Some(40000), unicode: true }), "CLOB CHARACTER SET UNICODE");
        assert_eq!(r(L::Varchar { len: Some(40000), unicode: false }), "VARCHAR(40000)");
        assert_eq!(r(L::Text { unicode: false }), "CLOB");
        assert_eq!(r(L::Binary { len: Some(16) }), "BYTE(16)");
        assert_eq!(r(L::Varbinary { len: Some(100) }), "VARBYTE(100)");
        assert_eq!(r(L::Blob), "BLOB");
        assert_eq!(r(L::Bit { len: Some(1) }), "BYTEINT");
        assert_eq!(r(L::Bit { len: Some(10) }), "VARBYTE(2)");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: Some(3), tz: true }), "TIME(3) WITH TIME ZONE");
        assert_eq!(r(L::Timestamp { precision: None, tz: false }), "TIMESTAMP");
        assert_eq!(r(L::Timestamp { precision: Some(7), tz: false }), "TIMESTAMP(6)");
        assert!(Teradata.render_type(&L::Timestamp { precision: Some(7), tz: false }).notes.iter().any(|n| n.code == IssueCode::PrecisionLoss));
        assert_eq!(r(L::Interval), "INTERVAL DAY(4) TO SECOND(6)");
        assert_eq!(r(L::Year), "SMALLINT");
        assert_eq!(r(L::Uuid), "CHAR(36)");
        assert_eq!(r(L::Json { binary: true }), "JSON(8388096) CHARACTER SET UNICODE");
        assert_eq!(r(L::Xml), "XML");
        assert_eq!(r(L::Enum { values: vec!["abc".into()] }), "VARCHAR(3) CHARACTER SET UNICODE");
        assert_eq!(r(L::Set { values: vec!["abc".into()] }), "VARCHAR(3) CHARACTER SET UNICODE");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }), "JSON(8388096) CHARACTER SET UNICODE");
        assert_eq!(r(L::Map { key: Box::new(L::int(4)), value: Box::new(L::int(4)) }), "JSON(8388096) CHARACTER SET UNICODE");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "ST_GEOMETRY");
        assert_eq!(r(L::Inet), "VARCHAR(45)");
        assert_eq!(r(L::MacAddr), "VARCHAR(17)");
        assert_eq!(r(L::RowVersion), "BYTE(8)");
        assert_eq!(r(L::Other { native: "PERIOD(DATE)".into() }), "PERIOD(DATE)");
    }

    #[test]
    fn round_trips_its_own_spellings() {
        for t in [
            L::int(1),
            L::int(2),
            L::int(4),
            L::int(8),
            L::Decimal { precision: Some(12), scale: Some(2) },
            L::Decimal { precision: None, scale: None },
            L::Float { bytes: 8 },
            L::Char { len: Some(3), unicode: true },
            L::Varchar { len: Some(40), unicode: false },
            L::Varchar { len: Some(40), unicode: true },
            L::Text { unicode: true },
            L::Binary { len: Some(16) },
            L::Varbinary { len: Some(16) },
            L::Blob,
            L::Date,
            L::Time { precision: Some(3), tz: true },
            L::Timestamp { precision: Some(0), tz: false },
            L::Interval,
            L::Json { binary: false },
            L::Xml,
        ] {
            let native = r(t.clone());
            assert_eq!(p(&native), t, "{native}");
        }
    }

    #[test]
    fn defaults() {
        let d = |v: DefaultValue, t: L| Teradata.render_default(&v, &t);
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: Some(6), tz: false }).as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: Some(0), tz: false }).as_deref(), Some("CURRENT_TIMESTAMP(0)"));
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: None, tz: false }).as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(d(DefaultValue::CurrentDate, L::Date).as_deref(), Some("CURRENT_DATE"));
        assert_eq!(d(DefaultValue::CurrentTime, L::Time { precision: Some(0), tz: false }).as_deref(), Some("CURRENT_TIME(0)"));
        assert_eq!(d(DefaultValue::Bool(true), L::Bool).as_deref(), Some("1"));
        assert_eq!(d(DefaultValue::NewUuid, L::Uuid), None);
    }
}
