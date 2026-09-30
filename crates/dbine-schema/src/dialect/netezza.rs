//! IBM Netezza (Netezza Performance Server).
//!
//! PostgreSQL-derived names with its own limits: CHAR/VARCHAR are LATIN-9
//! up to 64000 bytes and NCHAR/NVARCHAR are UTF-8 up to 16000 characters;
//! there are no LOBs, no identity columns (sequences instead), no user
//! indexes, and keys are informational. Rows are spread across data
//! slices by the DISTRIBUTE ON columns, which [`Netezza::finalize`] takes
//! from the primary key.

use super::postgres::{longest, precision_loss};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct Netezza;

const MAX_LATIN: u32 = 64000;
const MAX_NCHARS: u32 = 16000;
const MAX_BYTES: u32 = 64000;
const MAX_DECIMAL: u32 = 38;
/// DISTRIBUTE ON takes up to four columns.
const MAX_DISTRIBUTION: usize = 4;
/// Table option of the ODBC driver's designer.
const DISTRIBUTE_ON: &str = "distribute_on";

impl Dialect for Netezza {
    fn id(&self) -> &'static str {
        "netezza"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "boolean" | "bool" => L::Bool,
            "byteint" | "int1" => L::int(1),
            "smallint" | "int2" => L::int(2),
            "integer" | "int" | "int4" => L::int(4),
            "bigint" | "int8" => L::int(8),
            "numeric" | "decimal" | "dec" => L::Decimal { precision: p(0).or(Some(18)), scale: p(1).or(Some(0)) },
            "real" | "float4" => L::Float { bytes: 4 },
            "double precision" | "double" | "float8" => L::Float { bytes: 8 },
            // FLOAT(p): 1–6 digits is a REAL.
            "float" => L::Float { bytes: if p(0).is_some_and(|d| d <= 6) { 4 } else { 8 } },
            "character" | "char" | "bpchar" => L::Char { len: p(0).or(Some(1)), unicode: false },
            "character varying" | "varchar" => L::Varchar { len: p(0), unicode: false },
            "national character" | "nchar" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "national character varying" | "nvarchar" => L::Varchar { len: p(0), unicode: true },
            "varbinary" | "binary varying" => L::Varbinary { len: p(0) },
            "date" => L::Date,
            "time" => L::Time { precision: Some(6), tz: t.with_tz },
            "timetz" => L::Time { precision: Some(6), tz: true },
            "timestamp" => L::Timestamp { precision: Some(6), tz: false },
            "interval" => L::Interval,
            "json" => L::Json { binary: false },
            "jsonb" => L::Json { binary: true },
            "st_geometry" => L::Geometry { kind: None, srid: None, geography: false },
            n if n.starts_with("interval") => L::Interval,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let long_text = |why: String| Rendered::exact(format!("NVARCHAR({MAX_NCHARS})")).with(Loss, LengthLoss, why);
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes: 1, unsigned: false } => Rendered::exact("BYTEINT"),
            L::Int { bytes, unsigned } => {
                let r = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 | 2 => Rendered::exact("SMALLINT"),
                    3 | 4 => Rendered::exact("INTEGER"),
                    8 => Rendered::exact("BIGINT"),
                    _ if *bytes == 8 => Rendered::exact("NUMERIC(20, 0)"),
                    _ => Rendered::exact("NUMERIC(38, 0)").with(Loss, RangeLoss, "Entero de 16 bytes como NUMERIC(38, 0): no entran los valores de 39 dígitos."),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, "Netezza no tiene enteros sin signo: se usa un tipo más grande con signo.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } if *p <= MAX_DECIMAL => Rendered::exact(format!("NUMERIC({p}, {})", scale.unwrap_or(0))),
            L::Decimal { precision: Some(p), scale } => Rendered::exact(format!("NUMERIC(38, {})", scale.unwrap_or(0).min(38)))
                .with(Loss, PrecisionLoss, format!("Netezza admite hasta 38 dígitos; el origen tiene {p}.")),
            L::Decimal { precision: None, .. } => Rendered::exact("NUMERIC(38, 10)")
                .with(Loss, PrecisionLoss, "El origen no fija la precisión: se usa NUMERIC(38, 10)."),
            L::Float { bytes: 4 } => Rendered::exact("REAL"),
            L::Float { .. } => Rendered::exact("DOUBLE PRECISION"),
            L::Money => Rendered::exact("NUMERIC(19, 4)").with(Info, TypeChanged, "Moneda como NUMERIC(19, 4)."),
            L::Char { len, unicode: false } => match len.unwrap_or(1) {
                n if n <= MAX_LATIN => Rendered::exact(format!("CHAR({n})")),
                n => Rendered::exact(format!("VARCHAR({MAX_LATIN})")).with(Loss, LengthLoss, format!("Netezza admite hasta {MAX_LATIN} bytes; el origen tiene {n}.")),
            },
            L::Char { len, unicode: true } => match len.unwrap_or(1) {
                n if n <= MAX_NCHARS => Rendered::exact(format!("NCHAR({n})")),
                n => long_text(format!("Netezza admite hasta {MAX_NCHARS} caracteres Unicode; el origen tiene {n}.")),
            },
            L::Varchar { len: Some(n), unicode: false } if *n <= MAX_LATIN => Rendered::exact(format!("VARCHAR({n})")),
            L::Varchar { len: Some(n), unicode: true } if *n <= MAX_NCHARS => Rendered::exact(format!("NVARCHAR({n})")),
            L::Varchar { len: Some(n), .. } => long_text(format!("Netezza admite hasta {MAX_NCHARS} caracteres Unicode por columna; el origen tiene {n}.")),
            L::Varchar { len: None, .. } | L::Text { .. } => {
                long_text("Netezza no tiene texto sin límite: se usa NVARCHAR(16000) y los valores más largos no entran.".into())
            }
            L::Binary { len } | L::Varbinary { len } => match len {
                Some(n) if *n <= MAX_BYTES => Rendered::exact(format!("VARBINARY({n})")),
                _ => Rendered::exact(format!("VARBINARY({MAX_BYTES})")).with(Loss, LengthLoss, "Netezza guarda hasta 64000 bytes por valor binario."),
            },
            L::Blob => Rendered::exact(format!("VARBINARY({MAX_BYTES})"))
                .with(Loss, LengthLoss, "Netezza no tiene binarios sin límite: se usa VARBINARY(64000)."),
            L::Bit { len: Some(1) } => Rendered::exact("BOOLEAN"),
            L::Bit { len } => self.render_type(&L::Varbinary { len: Some(len.map_or(MAX_BYTES, |n| n.div_ceil(8))) })
                .with(Warning, TypeApproximated, "Netezza no tiene cadenas de bits: se guardan como binario."),
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => Rendered::exact(if *tz { "TIMETZ" } else { "TIME" }).with_loss(precision_loss(*precision, 6)),
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact("TIMESTAMP").with_loss(precision_loss(*precision, 6));
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Netezza no tiene TIMESTAMP con zona: conviene convertir los valores a UTC al copiarlos.")
                } else {
                    r
                }
            }
            L::Interval => Rendered::exact("INTERVAL"),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            L::Uuid => Rendered::exact("CHAR(36)").with(Info, TypeChanged, "Netezza no tiene UUID: se guarda como texto de 36 caracteres."),
            L::Json { .. } => Rendered::exact(format!("NVARCHAR({MAX_NCHARS})"))
                .with(Info, TypeChanged, "JSON como NVARCHAR(16000) (los tipos JSON de NPS 11.1 no están en todas las versiones)."),
            L::Xml => Rendered::exact(format!("NVARCHAR({MAX_NCHARS})")).with(Info, TypeChanged, "Netezza no tiene tipo XML: se guarda como NVARCHAR."),
            L::Enum { values } | L::Set { values } => Rendered::exact(format!("NVARCHAR({})", longest(values)))
                .with(Warning, TypeApproximated, format!("Netezza no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } => Rendered::exact(format!("NVARCHAR({MAX_NCHARS})"))
                .with(Warning, TypeApproximated, "Netezza no tiene arreglos ni mapas: se guardan como JSON en texto."),
            L::Geometry { .. } => Rendered::exact(format!("VARBINARY({MAX_BYTES})"))
                .with(Warning, TypeApproximated, "Geometría como binario (WKB), el formato que usa el Spatial Toolkit de Netezza."),
            L::Inet => Rendered::exact("VARCHAR(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("VARBINARY(8)")
                .with(Warning, TypeApproximated, "Netezza no tiene versión de fila automática: queda como binario y no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp => Some(match ty {
                L::Date => "CURRENT_DATE".into(),
                L::Time { .. } => "CURRENT_TIME".into(),
                _ => "CURRENT_TIMESTAMP".into(),
            }),
            other => standard_default(other, ty, "CURRENT_TIMESTAMP", None, false),
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            // Informational only; referential actions aren't kept.
            foreign_keys: true,
            on_delete: &[],
            on_update: &[],
            indexes: false,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 128,
            case: IdentCase::Upper,
        }
    }

    /// Distribute on the primary key: rows spread evenly and joins on the
    /// key stay local. Without a key Netezza takes the first column.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        if t.options.contains_key(DISTRIBUTE_ON) {
            return;
        }
        let Some(pk) = t.primary_key.as_ref().filter(|k| !k.columns.is_empty() && k.columns.len() <= MAX_DISTRIBUTION) else {
            return;
        };
        let cols = pk.columns.join(", ");
        t.options.insert(DISTRIBUTE_ON.into(), cols.clone());
        report.push(
            Severity::Info,
            IssueCode::OptionAdded,
            &t.name,
            Some(DISTRIBUTE_ON),
            format!("Se distribuye por la clave primaria: DISTRIBUTE ON ({cols})."),
        );
    }
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Netezza = Netezza;
    (driver_id == "netezza").then_some(&D as &dyn Dialect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use dbine_driver::KeyDef;

    fn p(s: &str) -> L {
        Netezza.parse_type(&parse(s))
    }
    fn r(t: L) -> String {
        Netezza.render_type(&t).native
    }

    #[test]
    fn parses_catalog_spellings() {
        assert_eq!(p("BYTEINT"), L::int(1));
        assert_eq!(p("SMALLINT"), L::int(2));
        assert_eq!(p("INTEGER"), L::int(4));
        assert_eq!(p("BIGINT"), L::int(8));
        assert_eq!(p("NUMERIC(18,2)"), L::Decimal { precision: Some(18), scale: Some(2) });
        assert_eq!(p("REAL"), L::Float { bytes: 4 });
        assert_eq!(p("DOUBLE PRECISION"), L::Float { bytes: 8 });
        assert_eq!(p("FLOAT(5)"), L::Float { bytes: 4 });
        assert_eq!(p("CHARACTER(10)"), L::Char { len: Some(10), unicode: false });
        assert_eq!(p("CHARACTER VARYING(255)"), L::Varchar { len: Some(255), unicode: false });
        assert_eq!(p("NATIONAL CHARACTER(10)"), L::Char { len: Some(10), unicode: true });
        assert_eq!(p("NATIONAL CHARACTER VARYING(255)"), L::Varchar { len: Some(255), unicode: true });
        assert_eq!(p("NVARCHAR(255)"), L::Varchar { len: Some(255), unicode: true });
        assert_eq!(p("VARBINARY(100)"), L::Varbinary { len: Some(100) });
        assert_eq!(p("BOOLEAN"), L::Bool);
        assert_eq!(p("DATE"), L::Date);
        assert_eq!(p("TIME"), L::Time { precision: Some(6), tz: false });
        assert_eq!(p("TIME WITH TIME ZONE"), L::Time { precision: Some(6), tz: true });
        assert_eq!(p("TIMETZ"), L::Time { precision: Some(6), tz: true });
        assert_eq!(p("TIMESTAMP"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(p("INTERVAL"), L::Interval);
        assert_eq!(p("JSONB"), L::Json { binary: true });
        assert_eq!(p("ST_GEOMETRY(200)"), L::Geometry { kind: None, srid: None, geography: false });
        assert!(matches!(p("JSONPATH"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(1)), "BYTEINT");
        assert_eq!(r(L::Int { bytes: 1, unsigned: true }), "SMALLINT");
        assert_eq!(r(L::int(4)), "INTEGER");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "BIGINT");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "NUMERIC(20, 0)");
        assert_eq!(r(L::int(16)), "NUMERIC(38, 0)");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "NUMERIC(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(40), scale: Some(2) }), "NUMERIC(38, 2)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "NUMERIC(38, 10)");
        assert_eq!(r(L::Float { bytes: 4 }), "REAL");
        assert_eq!(r(L::Float { bytes: 8 }), "DOUBLE PRECISION");
        assert_eq!(r(L::Money), "NUMERIC(19, 4)");
        assert_eq!(r(L::Char { len: Some(3), unicode: false }), "CHAR(3)");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }), "NCHAR(3)");
        assert_eq!(r(L::Varchar { len: Some(40), unicode: false }), "VARCHAR(40)");
        assert_eq!(r(L::Varchar { len: Some(40), unicode: true }), "NVARCHAR(40)");
        assert_eq!(r(L::Varchar { len: Some(20000), unicode: true }), "NVARCHAR(16000)");
        assert_eq!(r(L::Text { unicode: true }), "NVARCHAR(16000)");
        assert!(Netezza.render_type(&L::Text { unicode: true }).notes.iter().any(|n| n.code == IssueCode::LengthLoss));
        assert_eq!(r(L::Binary { len: Some(16) }), "VARBINARY(16)");
        assert_eq!(r(L::Varbinary { len: Some(100) }), "VARBINARY(100)");
        assert_eq!(r(L::Blob), "VARBINARY(64000)");
        assert_eq!(r(L::Bit { len: Some(1) }), "BOOLEAN");
        assert_eq!(r(L::Bit { len: Some(9) }), "VARBINARY(2)");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: Some(3), tz: true }), "TIMETZ");
        assert_eq!(r(L::Timestamp { precision: Some(3), tz: true }), "TIMESTAMP");
        assert_eq!(r(L::Interval), "INTERVAL");
        assert_eq!(r(L::Year), "SMALLINT");
        assert_eq!(r(L::Uuid), "CHAR(36)");
        assert_eq!(r(L::Json { binary: true }), "NVARCHAR(16000)");
        assert_eq!(r(L::Xml), "NVARCHAR(16000)");
        assert_eq!(r(L::Enum { values: vec!["abc".into()] }), "NVARCHAR(3)");
        assert_eq!(r(L::Set { values: vec!["abc".into()] }), "NVARCHAR(3)");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }), "NVARCHAR(16000)");
        assert_eq!(r(L::Map { key: Box::new(L::int(4)), value: Box::new(L::int(4)) }), "NVARCHAR(16000)");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "VARBINARY(64000)");
        assert_eq!(r(L::Inet), "VARCHAR(45)");
        assert_eq!(r(L::MacAddr), "VARCHAR(17)");
        assert_eq!(r(L::RowVersion), "VARBINARY(8)");
        assert_eq!(r(L::Other { native: "JSONPATH".into() }), "JSONPATH");
    }

    #[test]
    fn defaults() {
        let d = |v: DefaultValue, t: L| Netezza.render_default(&v, &t);
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: None, tz: false }).as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(d(DefaultValue::CurrentDate, L::Date).as_deref(), Some("CURRENT_DATE"));
        assert_eq!(d(DefaultValue::CurrentTime, L::Time { precision: None, tz: false }).as_deref(), Some("CURRENT_TIME"));
        assert_eq!(d(DefaultValue::Bool(true), L::Bool).as_deref(), Some("TRUE"));
        assert_eq!(d(DefaultValue::NewUuid, L::Uuid), None);
    }

    #[test]
    fn distributes_on_the_primary_key() {
        let mut t = TableSchema { name: "T".into(), primary_key: Some(KeyDef { name: None, columns: vec!["ID".into()] }), ..Default::default() };
        let mut report = Report::default();
        Netezza.finalize(&mut t, &mut report);
        assert_eq!(t.options.get(DISTRIBUTE_ON).map(String::as_str), Some("ID"));
        assert_eq!(report.issues[0].code, IssueCode::OptionAdded);
        // Without a key: Netezza's default.
        let mut t = TableSchema { name: "U".into(), ..Default::default() };
        Netezza.finalize(&mut t, &mut report);
        assert!(t.options.is_empty());
    }
}
