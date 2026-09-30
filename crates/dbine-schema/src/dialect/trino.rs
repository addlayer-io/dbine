//! Trino, Starburst and Presto (one type system: `varchar(n)`, `real`,
//! `timestamp(p) with time zone` to the picosecond, `array(T)`,
//! `map(K, V)`, `row(…)`, `uuid`, `ipaddress`), and Athena, whose queries
//! are Trino's but whose DDL is Hive's (see `spark.rs`).
//!
//! Trino has no primary keys, foreign keys, indexes or auto-increment.
//! What a table can hold depends on the catalog's connector: the Hive
//! connector takes no NOT NULL and few types, Iceberg has no
//! tinyint / smallint / char, memory takes almost everything. The dialect
//! renders the general type names; the report says what the catalog may
//! still refuse.

use super::bigquery::nested;
use super::postgres::{longest, precision_loss, prec};
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::{quote, DefaultValue};
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct Trino {
    /// Presto: no column defaults.
    presto: bool,
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static TRINO: Trino = Trino { presto: false };
    static PRESTO: Trino = Trino { presto: true };
    match driver_id {
        "trino" | "starburst" => Some(&TRINO),
        "presto" => Some(&PRESTO),
        "athena" => Some(super::spark::athena()),
        _ => None,
    }
}

/// Longest `char(n)`.
const MAX_CHAR: u32 = 65_536;

impl Dialect for Trino {
    fn id(&self) -> &'static str {
        "trino"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "boolean" => L::Bool,
            "tinyint" => L::int(1),
            "smallint" => L::int(2),
            "integer" | "int" => L::int(4),
            "bigint" => L::int(8),
            "real" | "float" => L::Float { bytes: 4 },
            "double" | "double precision" => L::Float { bytes: 8 },
            "decimal" | "numeric" => L::Decimal { precision: p(0).or(Some(38)), scale: p(1).or(Some(0)) },
            "varchar" | "char varying" | "character varying" => match p(0) {
                // `varchar(2147483647)` is how some connectors report the unbounded one.
                Some(n) if n < i32::MAX as u32 => L::Varchar { len: Some(n), unicode: true },
                _ => L::Text { unicode: true },
            },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varbinary" => L::Blob,
            "json" => L::Json { binary: false },
            "date" => L::Date,
            "time" => L::Time { precision: Some(p(0).unwrap_or(3) as u8), tz: t.with_tz },
            "timestamp" => L::Timestamp { precision: Some(p(0).unwrap_or(3) as u8), tz: t.with_tz },
            "uuid" => L::Uuid,
            "ipaddress" => L::Inet,
            "array" if t.args.len() == 1 => L::Array { of: Box::new(nested(self, &t.args[0])) },
            "map" if t.args.len() == 2 => L::Map { key: Box::new(nested(self, &t.args[0])), value: Box::new(nested(self, &t.args[1])) },
            // A record: the closest neutral type is a document.
            "row" => L::Json { binary: true },
            "geometry" | "sphericalgeography" => L::Geometry { kind: None, srid: None, geography: t.name != "geometry" },
            _ if t.name.starts_with("interval") => L::Interval,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("boolean"),
            L::Int { bytes, unsigned } => {
                let r = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 => Rendered::exact("tinyint"),
                    2 => Rendered::exact("smallint"),
                    3 | 4 => Rendered::exact("integer"),
                    8 => Rendered::exact("bigint"),
                    _ if *bytes == 8 => Rendered::exact("decimal(20, 0)"),
                    _ => Rendered::exact("decimal(38, 0)").with(Loss, RangeLoss, "Entero de 16 bytes como decimal(38, 0): no entran los valores de 39 dígitos."),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, "Trino no tiene enteros sin signo: se usa un tipo más grande con signo.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } if *p <= 38 => Rendered::exact(format!("decimal({p}, {})", scale.unwrap_or(0))),
            L::Decimal { precision: Some(p), scale } => Rendered::exact(format!("decimal(38, {})", scale.unwrap_or(0).min(38)))
                .with(Loss, PrecisionLoss, format!("Trino admite hasta 38 dígitos; el origen tiene {p}.")),
            L::Decimal { precision: None, .. } => Rendered::exact("decimal(38, 10)")
                .with(Loss, PrecisionLoss, "El origen no fija la precisión: se usa decimal(38, 10)."),
            L::Float { bytes: 4 } => Rendered::exact("real"),
            L::Float { .. } => Rendered::exact("double"),
            L::Money => Rendered::exact("decimal(19, 4)").with(Info, TypeChanged, "Moneda como decimal(19, 4)."),
            L::Char { len, .. } => match len.unwrap_or(1) {
                n if n <= MAX_CHAR => Rendered::exact(format!("char({n})")),
                n => Rendered::exact(format!("varchar({n})")).with(Info, TypeChanged, "char admite hasta 65536: se usa varchar."),
            },
            L::Varchar { len: Some(n), .. } => Rendered::exact(format!("varchar({n})")),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact("varchar"),
            L::Binary { len: Some(_) } | L::Varbinary { len: Some(_) } => {
                Rendered::exact("varbinary").with(Info, LengthLoss, "varbinary no limita el largo.")
            }
            L::Binary { len: None } | L::Varbinary { len: None } | L::Blob => Rendered::exact("varbinary"),
            L::Bit { .. } => Rendered::exact("varbinary").with(Warning, TypeApproximated, "Trino no tiene cadenas de bits: se guarda como binario."),
            L::Date => Rendered::exact("date"),
            L::Time { precision, tz } => Rendered::exact(format!("time{}{}", prec(Some(precision.unwrap_or(6)), 12), if *tz { " with time zone" } else { "" }))
                .with_loss(precision_loss(*precision, 12)),
            L::Timestamp { precision, tz } => {
                Rendered::exact(format!("timestamp{}{}", prec(Some(precision.unwrap_or(6)), 12), if *tz { " with time zone" } else { "" }))
                    .with_loss(precision_loss(*precision, 12))
            }
            L::Interval => Rendered::exact("varchar(64)").with(Warning, TypeApproximated, "Los conectores de Trino no guardan intervalos: queda como texto."),
            L::Year => Rendered::exact("smallint").with(Info, TypeChanged, "Año como smallint."),
            L::Uuid => Rendered::exact("uuid"),
            L::Json { .. } => Rendered::exact("json").with(Info, TypeChanged, "No todos los conectores guardan json (Hive e Iceberg no): ahí va varchar."),
            L::Xml => Rendered::exact("varchar").with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } => Rendered::exact(format!("varchar({})", longest(values)))
                .with(Warning, TypeApproximated, format!("Trino no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => Rendered::exact("array(varchar)")
                .with(Warning, TypeApproximated, format!("Conjunto como arreglo de texto. Valores: {}.", values.join(", "))),
            L::Array { of } => {
                let inner = self.render_type(of);
                Rendered { native: format!("array({})", inner.native), notes: inner.notes }
            }
            L::Map { key, value } => {
                let (k, v) = (self.render_type(key), self.render_type(value));
                let mut notes = k.notes;
                notes.extend(v.notes);
                Rendered { native: format!("map({}, {})", k.native, v.native), notes }
            }
            L::Geometry { .. } => Rendered::exact("varchar").with(Warning, TypeApproximated, "Los conectores de Trino no guardan geometrías: queda como texto (WKT)."),
            L::Inet => Rendered::exact("ipaddress").with(Info, TypeChanged, "ipaddress no guarda máscaras de red."),
            L::MacAddr => Rendered::exact("varchar(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("varbinary").with(Warning, TypeApproximated, "Trino no tiene versión de fila automática."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    /// Column defaults are literals only (no `current_timestamp`, no
    /// function calls), and only where the connector supports them.
    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::Null => "NULL".into(),
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => quote(s),
            DefaultValue::Bool(b) if matches!(ty, L::Bool) => if *b { "true" } else { "false" }.into(),
            DefaultValue::Bool(b) => if *b { "1" } else { "0" }.into(),
            DefaultValue::CurrentTimestamp
            | DefaultValue::CurrentDate
            | DefaultValue::CurrentTime
            | DefaultValue::NewUuid
            | DefaultValue::NextVal(_)
            | DefaultValue::Expr(_) => return None,
        })
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: false,
            on_delete: &[],
            on_update: &[],
            indexes: false,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: !self.presto,
            nullability: true,
            comments: true,
            max_identifier: 128,
            case: IdentCase::Lower,
        }
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        if let Some(k) = t.primary_key.take().filter(|k| !k.columns.is_empty()) {
            report.push(
                Severity::Dropped,
                IssueCode::PrimaryKeyDropped,
                &t.name,
                None,
                format!("Trino no tiene claves primarias: se omite ({}).", k.columns.join(", ")),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::logical_of;
    use crate::parse::parse;

    fn lt(s: &str) -> L {
        logical_of(lookup("trino").unwrap(), &parse(s))
    }

    #[test]
    fn ids() {
        for id in ["trino", "presto", "starburst", "athena"] {
            assert!(lookup(id).is_some(), "{id}");
        }
        assert!(lookup("trino").unwrap().caps().defaults);
        assert!(!lookup("presto").unwrap().caps().defaults);
        assert_eq!(lookup("athena").unwrap().id(), "athena");
    }

    #[test]
    fn parses_catalog_spellings() {
        assert_eq!(lt("bigint"), L::int(8));
        assert_eq!(lt("real"), L::Float { bytes: 4 });
        assert_eq!(lt("decimal(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(lt("varchar(20)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(lt("varchar"), L::Text { unicode: true });
        assert_eq!(lt("varchar(2147483647)"), L::Text { unicode: true });
        assert_eq!(lt("char(3)"), L::Char { len: Some(3), unicode: true });
        assert_eq!(lt("varbinary"), L::Blob);
        assert_eq!(lt("timestamp(3)"), L::Timestamp { precision: Some(3), tz: false });
        assert_eq!(lt("timestamp(6) with time zone"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(lt("time(3) with time zone"), L::Time { precision: Some(3), tz: true });
        assert_eq!(lt("interval day to second"), L::Interval);
        assert_eq!(lt("array(varchar)"), L::Array { of: Box::new(L::Text { unicode: true }) });
        assert_eq!(lt("map(varchar, integer)"), L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::int(4)) });
        assert_eq!(lt("row(a integer, b varchar)"), L::Json { binary: true });
        assert_eq!(lt("uuid"), L::Uuid);
        assert_eq!(lt("ipaddress"), L::Inet);
        assert_eq!(lt("json"), L::Json { binary: false });
        assert!(matches!(lt("hyperloglog"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let d = lookup("trino").unwrap();
        let r = |t: L| d.render_type(&t);
        assert_eq!(r(L::Bool).native, "boolean");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }).native, "bigint");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }).native, "decimal(20, 0)");
        assert_eq!(r(L::int(16)).notes[0].code, IssueCode::RangeLoss);
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }).native, "decimal(12, 2)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }).native, "decimal(38, 10)");
        assert_eq!(r(L::Float { bytes: 4 }).native, "real");
        assert_eq!(r(L::Float { bytes: 8 }).native, "double");
        assert_eq!(r(L::Money).native, "decimal(19, 4)");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }).native, "char(3)");
        assert_eq!(r(L::Varchar { len: Some(30), unicode: true }).native, "varchar(30)");
        assert_eq!(r(L::Text { unicode: true }).native, "varchar");
        assert_eq!(r(L::Binary { len: Some(16) }).native, "varbinary");
        assert_eq!(r(L::Blob).native, "varbinary");
        assert_eq!(r(L::Bit { len: None }).native, "varbinary");
        assert_eq!(r(L::Date).native, "date");
        assert_eq!(r(L::Time { precision: None, tz: false }).native, "time(6)");
        assert_eq!(r(L::Timestamp { precision: Some(3), tz: true }).native, "timestamp(3) with time zone");
        assert_eq!(r(L::Timestamp { precision: None, tz: false }).native, "timestamp(6)");
        assert_eq!(r(L::Interval).native, "varchar(64)");
        assert_eq!(r(L::Year).native, "smallint");
        assert_eq!(r(L::Uuid).native, "uuid");
        assert_eq!(r(L::Json { binary: true }).native, "json");
        assert_eq!(r(L::Xml).native, "varchar");
        assert_eq!(r(L::Enum { values: vec!["abc".into()] }).native, "varchar(3)");
        assert_eq!(r(L::Set { values: vec!["a".into()] }).native, "array(varchar)");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }).native, "array(integer)");
        assert_eq!(r(L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::int(8)) }).native, "map(varchar, bigint)");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }).native, "varchar");
        assert_eq!(r(L::Inet).native, "ipaddress");
        assert_eq!(r(L::MacAddr).native, "varchar(17)");
        assert_eq!(r(L::RowVersion).native, "varbinary");
    }

    #[test]
    fn defaults_and_finalize() {
        let d = lookup("trino").unwrap();
        assert_eq!(d.render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: false }), None);
        assert_eq!(d.render_default(&DefaultValue::Number("0".into()), &L::int(4)).as_deref(), Some("0"));
        assert_eq!(d.render_default(&DefaultValue::Text("it's".into()), &L::Text { unicode: true }).as_deref(), Some("'it''s'"));
        let mut t = TableSchema {
            name: "t".into(),
            primary_key: Some(dbine_driver::KeyDef { name: None, columns: vec!["id".into()] }),
            ..Default::default()
        };
        let mut rep = Report::default();
        d.finalize(&mut t, &mut rep);
        assert!(t.primary_key.is_none());
        assert!(rep.issues.iter().any(|i| i.code == IssueCode::PrimaryKeyDropped));
    }
}
