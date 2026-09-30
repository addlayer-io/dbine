//! Exasol.
//!
//! Exact numbers are all DECIMAL (INTEGER is DECIMAL(18,0), BIGINT
//! DECIMAL(36,0)); approximate ones are DOUBLE. Text is CHAR/VARCHAR in
//! UTF8 or ASCII, measured in characters, up to 2,000,000. There are no
//! binary types (HASHTYPE holds fixed-length hashes and UUIDs), no TIME,
//! no user indexes and no referential actions.

use super::postgres::{longest, precision_loss, prec};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;

pub struct Exasol;

const MAX_CHAR: u32 = 2000;
const MAX_VARCHAR: u32 = 2_000_000;
const MAX_DECIMAL: u32 = 36;
const MAX_FRACTION: u8 = 9;
const MAX_HASHTYPE: u32 = 1024;

/// DECIMAL(p, 0) as the smallest integer that holds it.
fn integer_of(p: u32) -> Option<L> {
    Some(match p {
        0..=2 => L::int(1),
        3..=4 => L::int(2),
        5..=9 => L::int(4),
        10..=18 => L::int(8),
        _ => return None,
    })
}

impl Dialect for Exasol {
    fn id(&self) -> &'static str {
        "exasol"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let unicode = !t.has("ascii");
        // `HASHTYPE(16 BYTE)` / `HASHTYPE(128 BIT)`.
        let hash_bytes = || {
            let a = t.args.first()?.to_ascii_lowercase();
            let mut w = a.split_whitespace();
            let n: u32 = w.next()?.parse().ok()?;
            Some(if w.next() == Some("bit") { n / 8 } else { n })
        };
        match t.name.as_str() {
            "boolean" | "bool" => L::Bool,
            "decimal" | "dec" | "numeric" | "number" => {
                let (pr, s) = (p(0).unwrap_or(18), p(1).unwrap_or(0));
                match (s, integer_of(pr)) {
                    (0, Some(i)) => i,
                    _ => L::Decimal { precision: Some(pr), scale: Some(s) },
                }
            }
            // Aliases, stored as the DECIMAL they stand for.
            "tinyint" => L::int(2),
            "smallint" | "shortint" => L::int(4),
            "integer" | "int" => L::int(8),
            "bigint" => L::Decimal { precision: Some(36), scale: Some(0) },
            "double" | "double precision" | "float" | "real" => L::Float { bytes: 8 },
            "char" | "character" | "nchar" => L::Char { len: p(0).or(Some(1)), unicode },
            "varchar" | "character varying" | "varchar2" | "nvarchar" | "nvarchar2" => L::Varchar { len: p(0), unicode },
            "clob" | "long varchar" => L::Varchar { len: Some(MAX_VARCHAR), unicode },
            "date" => L::Date,
            "timestamp" => L::Timestamp { precision: Some(p(0).unwrap_or(3) as u8), tz: t.local_tz },
            "hashtype" => match hash_bytes() {
                Some(16) => L::Uuid,
                n => L::Binary { len: n },
            },
            "geometry" => L::Geometry { kind: None, srid: p(0), geography: false },
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
                let digits = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 => 3,
                    2 => 5,
                    3 | 4 => 10,
                    8 => 19,
                    _ if *bytes == 8 => 20,
                    _ => 39,
                };
                if digits <= MAX_DECIMAL {
                    Rendered::exact(format!("DECIMAL({digits}, 0)"))
                } else {
                    Rendered::exact("DECIMAL(36, 0)").with(Loss, RangeLoss, "Entero de 16 bytes: Exasol admite hasta 36 dígitos.")
                }
            }
            L::Decimal { precision: Some(p), scale } if *p <= MAX_DECIMAL => Rendered::exact(format!("DECIMAL({p}, {})", scale.unwrap_or(0))),
            L::Decimal { precision: Some(p), scale } => Rendered::exact(format!("DECIMAL({MAX_DECIMAL}, {})", scale.unwrap_or(0).min(MAX_DECIMAL)))
                .with(Loss, PrecisionLoss, format!("Exasol admite hasta {MAX_DECIMAL} dígitos; el origen tiene {p}.")),
            L::Decimal { precision: None, .. } => Rendered::exact("DECIMAL(36, 10)")
                .with(Loss, PrecisionLoss, "El origen no fija la precisión: se usa DECIMAL(36, 10)."),
            L::Float { .. } => Rendered::exact("DOUBLE PRECISION"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(Info, TypeChanged, "Moneda como DECIMAL(19, 4)."),
            L::Char { len, unicode } => {
                let cs = if *unicode { "UTF8" } else { "ASCII" };
                match len.unwrap_or(1) {
                    n if n <= MAX_CHAR => Rendered::exact(format!("CHAR({n}) {cs}")),
                    n => Rendered::exact(format!("VARCHAR({}) {cs}", n.min(MAX_VARCHAR))).with(Info, TypeChanged, format!("CHAR admite hasta {MAX_CHAR}: se usa VARCHAR.")),
                }
            }
            L::Varchar { len: Some(n), unicode } if *n <= MAX_VARCHAR => {
                Rendered::exact(format!("VARCHAR({n}) {}", if *unicode { "UTF8" } else { "ASCII" }))
            }
            L::Varchar { len: Some(n), .. } => Rendered::exact(format!("VARCHAR({MAX_VARCHAR}) UTF8"))
                .with(Loss, LengthLoss, format!("Exasol admite hasta {MAX_VARCHAR} caracteres; el origen tiene {n}.")),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact(format!("VARCHAR({MAX_VARCHAR}) UTF8"))
                .with(Warning, LengthLoss, "Texto sin límite como VARCHAR(2000000): Exasol guarda hasta 2.000.000 caracteres."),
            L::Binary { len: Some(n) } if *n <= MAX_HASHTYPE => Rendered::exact(format!("HASHTYPE({n} BYTE)"))
                .with(Info, TypeChanged, "Binario de largo fijo como HASHTYPE: se lee y escribe en hexadecimal."),
            L::Binary { len } | L::Varbinary { len } => Rendered::exact(format!("VARCHAR({}) ASCII", len.map_or(MAX_VARCHAR, |n| n.saturating_mul(2).min(MAX_VARCHAR))))
                .with(Warning, TypeApproximated, "Exasol no tiene tipos binarios: se guardan en hexadecimal como texto."),
            L::Blob => Rendered::exact(format!("VARCHAR({MAX_VARCHAR}) ASCII"))
                .with(Warning, TypeApproximated, "Exasol no tiene tipos binarios: se guardan en hexadecimal como texto (hasta 1.000.000 de bytes)."),
            L::Bit { len: Some(1) } => Rendered::exact("BOOLEAN"),
            L::Bit { len } => Rendered::exact(format!("VARCHAR({}) ASCII", len.unwrap_or(MAX_VARCHAR).min(MAX_VARCHAR)))
                .with(Warning, TypeApproximated, "Exasol no tiene cadenas de bits: se guardan como texto de ceros y unos."),
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => {
                let r = Rendered::exact(format!("TIMESTAMP{}", prec(*precision, MAX_FRACTION)))
                    .with(Warning, TypeApproximated, "Exasol no tiene tipo hora: se guarda como TIMESTAMP, con una fecha.");
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Exasol no guarda la zona horaria de una hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz: false } => Rendered::exact(format!("TIMESTAMP{}", prec(*precision, MAX_FRACTION)))
                .with_loss(precision_loss(*precision, MAX_FRACTION)),
            L::Timestamp { precision, tz: true } => Rendered::exact(format!("TIMESTAMP{} WITH LOCAL TIME ZONE", prec(*precision, MAX_FRACTION)))
                .with_loss(precision_loss(*precision, MAX_FRACTION))
                .with(Info, TimeZoneLoss, "WITH LOCAL TIME ZONE guarda el instante y lo muestra en la zona de la sesión; no conserva la zona original de cada valor."),
            L::Interval => Rendered::exact("INTERVAL DAY(9) TO SECOND(6)")
                .with(Warning, TypeApproximated, "Intervalo de días a segundos: los años y meses van en INTERVAL YEAR TO MONTH en Exasol."),
            L::Year => Rendered::exact("DECIMAL(4, 0)").with(Info, TypeChanged, "Año como DECIMAL(4, 0)."),
            L::Uuid => Rendered::exact("HASHTYPE(16 BYTE)"),
            L::Json { .. } => Rendered::exact(format!("VARCHAR({MAX_VARCHAR}) UTF8"))
                .with(Info, TypeChanged, "Exasol no tiene tipo JSON: se guarda como VARCHAR (JSON_VALUE y JSON_EXTRACT trabajan sobre él)."),
            L::Xml => Rendered::exact(format!("VARCHAR({MAX_VARCHAR}) UTF8")).with(Info, TypeChanged, "Exasol no tiene tipo XML: se guarda como VARCHAR."),
            L::Enum { values } | L::Set { values } => Rendered::exact(format!("VARCHAR({}) UTF8", longest(values)))
                .with(Warning, TypeApproximated, format!("Exasol no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } => Rendered::exact(format!("VARCHAR({MAX_VARCHAR}) UTF8"))
                .with(Warning, TypeApproximated, "Exasol no tiene arreglos ni mapas: se guardan como JSON en texto."),
            L::Geometry { srid, geography, .. } => match srid.or(geography.then_some(4326)) {
                Some(s) => Rendered::exact(format!("GEOMETRY({s})")),
                None => Rendered::exact("GEOMETRY"),
            },
            L::Inet => Rendered::exact("VARCHAR(45) ASCII").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR(17) ASCII").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("HASHTYPE(8 BYTE)")
                .with(Warning, TypeApproximated, "Exasol no tiene versión de fila automática: queda como binario y no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp => Some(match ty {
                L::Date => "CURRENT_DATE".into(),
                L::Timestamp { tz: true, .. } => "CURRENT_TIMESTAMP".into(),
                _ => "LOCALTIMESTAMP".into(),
            }),
            // TIME is stored as a TIMESTAMP.
            DefaultValue::CurrentTime => Some("LOCALTIMESTAMP".into()),
            other => standard_default(other, ty, "LOCALTIMESTAMP", None, false),
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            on_delete: &[],
            on_update: &[],
            // Exasol builds and drops its indexes on its own.
            indexes: false,
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

    fn implies_auto_increment(&self, t: &TypeSpec) -> bool {
        t.has("identity")
    }
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Exasol = Exasol;
    (driver_id == "exasol").then_some(&D as &dyn Dialect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn p(s: &str) -> L {
        Exasol.parse_type(&parse(s))
    }
    fn r(t: L) -> String {
        Exasol.render_type(&t).native
    }

    #[test]
    fn parses_catalog_spellings() {
        assert_eq!(p("DECIMAL(18,0)"), L::int(8));
        assert_eq!(p("DECIMAL(9,0)"), L::int(4));
        assert_eq!(p("DECIMAL(3,0)"), L::int(2));
        assert_eq!(p("DECIMAL(36,0)"), L::Decimal { precision: Some(36), scale: Some(0) });
        assert_eq!(p("DECIMAL(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(p("INTEGER"), L::int(8));
        assert_eq!(p("BIGINT"), L::Decimal { precision: Some(36), scale: Some(0) });
        assert_eq!(p("DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(p("DOUBLE PRECISION"), L::Float { bytes: 8 });
        assert_eq!(p("BOOLEAN"), L::Bool);
        assert_eq!(p("CHAR(10) UTF8"), L::Char { len: Some(10), unicode: true });
        assert_eq!(p("CHAR(10) ASCII"), L::Char { len: Some(10), unicode: false });
        assert_eq!(p("VARCHAR(2000000) UTF8"), L::Varchar { len: Some(2_000_000), unicode: true });
        assert_eq!(p("VARCHAR(100)"), L::Varchar { len: Some(100), unicode: true });
        assert_eq!(p("DATE"), L::Date);
        assert_eq!(p("TIMESTAMP"), L::Timestamp { precision: Some(3), tz: false });
        assert_eq!(p("TIMESTAMP(6)"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(p("TIMESTAMP WITH LOCAL TIME ZONE"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(p("TIMESTAMP(6) WITH LOCAL TIME ZONE"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(p("INTERVAL YEAR(2) TO MONTH"), L::Interval);
        assert_eq!(p("INTERVAL DAY(2) TO SECOND(3)"), L::Interval);
        assert_eq!(p("GEOMETRY(4326)"), L::Geometry { kind: None, srid: Some(4326), geography: false });
        assert_eq!(p("GEOMETRY"), L::Geometry { kind: None, srid: None, geography: false });
        assert_eq!(p("HASHTYPE(16 BYTE)"), L::Uuid);
        assert_eq!(p("HASHTYPE(256 BIT)"), L::Binary { len: Some(32) });
        assert!(matches!(p("SOMETHING"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(1)), "DECIMAL(3, 0)");
        assert_eq!(r(L::int(2)), "DECIMAL(5, 0)");
        assert_eq!(r(L::int(4)), "DECIMAL(10, 0)");
        assert_eq!(r(L::int(8)), "DECIMAL(19, 0)");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "DECIMAL(20, 0)");
        assert_eq!(r(L::int(16)), "DECIMAL(36, 0)");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "DECIMAL(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(38), scale: Some(2) }), "DECIMAL(36, 2)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "DECIMAL(36, 10)");
        assert_eq!(r(L::Float { bytes: 4 }), "DOUBLE PRECISION");
        assert_eq!(r(L::Money), "DECIMAL(19, 4)");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }), "CHAR(3) UTF8");
        assert_eq!(r(L::Char { len: Some(3000), unicode: true }), "VARCHAR(3000) UTF8");
        assert_eq!(r(L::Varchar { len: Some(40), unicode: false }), "VARCHAR(40) ASCII");
        assert_eq!(r(L::Text { unicode: true }), "VARCHAR(2000000) UTF8");
        assert_eq!(r(L::Binary { len: Some(16) }), "HASHTYPE(16 BYTE)");
        assert_eq!(r(L::Varbinary { len: Some(100) }), "VARCHAR(200) ASCII");
        assert_eq!(r(L::Blob), "VARCHAR(2000000) ASCII");
        assert_eq!(r(L::Bit { len: Some(1) }), "BOOLEAN");
        assert_eq!(r(L::Bit { len: Some(12) }), "VARCHAR(12) ASCII");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: None, tz: false }), "TIMESTAMP");
        assert_eq!(r(L::Timestamp { precision: Some(6), tz: false }), "TIMESTAMP(6)");
        assert_eq!(r(L::Timestamp { precision: Some(6), tz: true }), "TIMESTAMP(6) WITH LOCAL TIME ZONE");
        assert_eq!(r(L::Interval), "INTERVAL DAY(9) TO SECOND(6)");
        assert_eq!(r(L::Year), "DECIMAL(4, 0)");
        assert_eq!(r(L::Uuid), "HASHTYPE(16 BYTE)");
        assert_eq!(r(L::Json { binary: true }), "VARCHAR(2000000) UTF8");
        assert_eq!(r(L::Xml), "VARCHAR(2000000) UTF8");
        assert_eq!(r(L::Enum { values: vec!["abc".into()] }), "VARCHAR(3) UTF8");
        assert_eq!(r(L::Set { values: vec!["abc".into()] }), "VARCHAR(3) UTF8");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }), "VARCHAR(2000000) UTF8");
        assert_eq!(r(L::Map { key: Box::new(L::int(4)), value: Box::new(L::int(4)) }), "VARCHAR(2000000) UTF8");
        assert_eq!(r(L::Geometry { kind: None, srid: Some(4326), geography: false }), "GEOMETRY(4326)");
        assert_eq!(r(L::Inet), "VARCHAR(45) ASCII");
        assert_eq!(r(L::MacAddr), "VARCHAR(17) ASCII");
        assert_eq!(r(L::RowVersion), "HASHTYPE(8 BYTE)");
        assert_eq!(r(L::Other { native: "X".into() }), "X");
    }

    #[test]
    fn round_trips_its_own_spellings() {
        for t in [
            L::Bool,
            L::Decimal { precision: Some(12), scale: Some(2) },
            L::Float { bytes: 8 },
            L::Char { len: Some(3), unicode: true },
            L::Varchar { len: Some(40), unicode: false },
            L::Date,
            L::Timestamp { precision: Some(6), tz: true },
            L::Uuid,
            L::Interval,
        ] {
            let native = r(t.clone());
            assert_eq!(p(&native), t, "{native}");
        }
        // Integers come back as the smallest integer their DECIMAL holds.
        assert_eq!(p(&r(L::int(8))), L::Decimal { precision: Some(19), scale: Some(0) });
        assert_eq!(p(&r(L::int(4))), L::int(8));
    }

    #[test]
    fn defaults() {
        let d = |v: DefaultValue, t: L| Exasol.render_default(&v, &t);
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: None, tz: false }).as_deref(), Some("LOCALTIMESTAMP"));
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: None, tz: true }).as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(d(DefaultValue::CurrentDate, L::Date).as_deref(), Some("CURRENT_DATE"));
        assert_eq!(d(DefaultValue::Bool(true), L::Bool).as_deref(), Some("TRUE"));
        assert_eq!(d(DefaultValue::NewUuid, L::Uuid), None);
    }
}
