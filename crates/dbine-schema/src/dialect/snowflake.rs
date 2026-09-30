//! Snowflake. Every number is NUMBER(p, s) (INTEGER is NUMBER(38, 0)),
//! every text VARCHAR (up to 16 MB, the catalog says TEXT), binaries up to
//! 8 MB; three timestamps: NTZ (no zone), LTZ (an instant shown in the
//! session's zone) and TZ (keeps the offset); VARIANT / OBJECT / ARRAY
//! hold semi-structured data. Keys are informational except NOT NULL;
//! there are no indexes, only UNIQUE constraints (not enforced either).

use super::bigquery::{generic, nested, not_enforced};
use super::postgres::{longest, precision_loss, prec};
use super::{Caps, Dialect, IdentCase, Rendered, ALL_ACTIONS};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct Snowflake;

/// VARCHAR's maximum, in characters (the catalog reports it for plain VARCHAR).
const MAX_VARCHAR: u32 = 16_777_216;
/// BINARY's maximum, in bytes.
const MAX_BINARY: u32 = 8_388_608;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Snowflake = Snowflake;
    (driver_id == "snowflake").then_some(&D as &dyn Dialect)
}

/// A Snowflake string literal (backslash is an escape there too).
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

impl Dialect for Snowflake {
    fn id(&self) -> &'static str {
        "snowflake"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let ts = |tz| L::Timestamp { precision: Some(p(0).unwrap_or(9) as u8), tz };
        match t.name.as_str() {
            "number" | "numeric" | "decimal" | "dec" => {
                let (pr, sc) = (p(0).unwrap_or(38), p(1).unwrap_or(0));
                match (pr, sc) {
                    // NUMBER(p, 0): a whole number of p digits.
                    (pr, 0) if pr <= 4 => L::int(2),
                    (pr, 0) if pr <= 9 => L::int(4),
                    (pr, 0) if pr <= 18 => L::int(8),
                    (pr, sc) => L::Decimal { precision: Some(pr), scale: Some(sc) },
                }
            }
            // All of them are NUMBER(38, 0).
            "int" | "integer" | "bigint" | "smallint" | "tinyint" | "byteint" => L::Decimal { precision: Some(38), scale: Some(0) },
            "float" | "float4" | "float8" | "double" | "double precision" | "real" => L::Float { bytes: 8 },
            "text" | "varchar" | "string" | "char" | "character" | "nchar" | "nvarchar" | "nvarchar2" | "char varying"
            | "nchar varying" => match p(0) {
                Some(n) if n >= MAX_VARCHAR => L::Text { unicode: true },
                Some(n) if matches!(t.name.as_str(), "char" | "character" | "nchar") => L::Char { len: Some(n), unicode: true },
                Some(n) => L::Varchar { len: Some(n), unicode: true },
                None if matches!(t.name.as_str(), "char" | "character" | "nchar") => L::Char { len: Some(1), unicode: true },
                None => L::Text { unicode: true },
            },
            "binary" | "varbinary" => match p(0) {
                Some(n) if n < MAX_BINARY => L::Varbinary { len: Some(n) },
                _ => L::Blob,
            },
            "boolean" | "bool" => L::Bool,
            "date" => L::Date,
            "time" => L::Time { precision: Some(p(0).unwrap_or(9) as u8), tz: false },
            "timestamp_ntz" | "datetime" | "timestamp" | "timestampntz" | "timestamp without time zone" => ts(false),
            "timestamp_ltz" | "timestamp_tz" | "timestampltz" | "timestamptz" | "timestamp with local time zone"
            | "timestamp with time zone" => ts(true),
            "variant" | "object" => L::Json { binary: true },
            // Semi-structured ARRAY (of VARIANT); structured ARRAY(T) below.
            "array" if t.args.is_empty() => L::Json { binary: true },
            "array" => L::Array { of: Box::new(nested(self, &t.args[0])) },
            "map" if t.args.len() == 2 => L::Map { key: Box::new(nested(self, &t.args[0])), value: Box::new(nested(self, &t.args[1])) },
            "geography" => L::Geometry { kind: None, srid: Some(4326), geography: true },
            "geometry" => L::Geometry { kind: None, srid: None, geography: false },
            _ => match generic(&t.raw) {
                Some((head, args)) if head == "array" && args.len() == 1 => L::Array { of: Box::new(nested(self, &args[0])) },
                _ => L::Other { native: t.raw.clone() },
            },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let text_note = |r: Rendered| r.with(Info, LengthLoss, "VARCHAR de Snowflake admite hasta 16 MB.");
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => {
                // Digits of the largest value.
                let digits = match (bytes, unsigned) {
                    (1, _) => 3,
                    (2, _) => 5,
                    (3, _) => 8,
                    (4, _) => 10,
                    (8, false) => 19,
                    (8, true) => 20,
                    _ => 38,
                };
                let r = Rendered::exact(format!("NUMBER({digits}, 0)"));
                if *bytes == 16 {
                    r.with(Loss, RangeLoss, "Entero de 16 bytes como NUMBER(38, 0): no entran los valores de 39 dígitos.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } if *p <= 38 => Rendered::exact(format!("NUMBER({p}, {})", scale.unwrap_or(0))),
            L::Decimal { precision: Some(p), scale } => Rendered::exact(format!("NUMBER(38, {})", scale.unwrap_or(0).min(37)))
                .with(Loss, PrecisionLoss, format!("Snowflake admite hasta 38 dígitos; el origen tiene {p}.")),
            L::Decimal { precision: None, .. } => Rendered::exact("NUMBER(38, 10)")
                .with(Loss, PrecisionLoss, "El origen no fija la precisión: se usa NUMBER(38, 10)."),
            L::Float { .. } => Rendered::exact("FLOAT"),
            L::Money => Rendered::exact("NUMBER(19, 4)").with(Info, TypeChanged, "Moneda como NUMBER(19, 4)."),
            L::Char { len, .. } => Rendered::exact(format!("CHAR({})", len.unwrap_or(1).min(MAX_VARCHAR)))
                .with(Info, TypeChanged, "CHAR de Snowflake no rellena con espacios."),
            L::Varchar { len: Some(n), .. } if *n < MAX_VARCHAR => Rendered::exact(format!("VARCHAR({n})")),
            L::Varchar { .. } | L::Text { .. } => text_note(Rendered::exact("VARCHAR")),
            L::Binary { len: Some(n) } | L::Varbinary { len: Some(n) } if *n <= MAX_BINARY => Rendered::exact(format!("BINARY({n})")),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("BINARY").with(Info, LengthLoss, "BINARY de Snowflake admite hasta 8 MB."),
            L::Bit { len } => Rendered::exact(len.map_or("BINARY".into(), |n| format!("BINARY({})", n.div_ceil(8).max(1))))
                .with(Warning, TypeApproximated, "Snowflake no tiene cadenas de bits: se guarda como binario."),
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => {
                let r = Rendered::exact(format!("TIME{}", prec(*precision, 9))).with_loss(precision_loss(*precision, 9));
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Snowflake no guarda la zona horaria de una hora.")
                } else {
                    r
                }
            }
            // TZ keeps the offset, so it holds what LTZ holds and more.
            L::Timestamp { precision, tz } => {
                let kind = if *tz { "TIMESTAMP_TZ" } else { "TIMESTAMP_NTZ" };
                Rendered::exact(format!("{kind}{}", prec(*precision, 9))).with_loss(precision_loss(*precision, 9))
            }
            L::Interval => Rendered::exact("VARCHAR(64)").with(Loss, TypeApproximated, "Snowflake no guarda intervalos en columnas: queda como texto."),
            L::Year => Rendered::exact("NUMBER(4, 0)").with(Info, TypeChanged, "Año como NUMBER(4, 0)."),
            L::Uuid => Rendered::exact("VARCHAR(36)").with(Info, TypeChanged, "UUID como VARCHAR(36) (Snowflake no tiene tipo UUID)."),
            L::Json { .. } => Rendered::exact("VARIANT").with(Info, TypeChanged, "JSON como VARIANT: se carga con PARSE_JSON."),
            L::Xml => Rendered::exact("VARCHAR").with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } => Rendered::exact(format!("VARCHAR({})", longest(values)))
                .with(Warning, TypeApproximated, format!("Snowflake no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => Rendered::exact("ARRAY")
                .with(Warning, TypeApproximated, format!("Conjunto como ARRAY. Valores: {}.", values.join(", "))),
            L::Array { .. } => Rendered::exact("ARRAY").with(Info, TypeChanged, "ARRAY de Snowflake guarda VARIANT: no fija el tipo de los elementos."),
            L::Map { .. } => Rendered::exact("OBJECT").with(Info, TypeChanged, "Mapa como OBJECT (claves de texto, valores VARIANT)."),
            L::Geometry { geography: true, .. } => Rendered::exact("GEOGRAPHY"),
            L::Geometry { .. } => Rendered::exact("GEOMETRY"),
            L::Inet => Rendered::exact("VARCHAR(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("BINARY(8)").with(Warning, TypeApproximated, "Snowflake no tiene versión de fila automática."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::Null => "NULL".into(),
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => literal(s),
            DefaultValue::Bool(b) if matches!(ty, L::Bool) => if *b { "TRUE" } else { "FALSE" }.into(),
            DefaultValue::Bool(b) => if *b { "1" } else { "0" }.into(),
            DefaultValue::CurrentTimestamp if matches!(ty, L::Date) => "CURRENT_DATE()".into(),
            DefaultValue::CurrentTimestamp => "CURRENT_TIMESTAMP()".into(),
            DefaultValue::CurrentDate => "CURRENT_DATE()".into(),
            DefaultValue::CurrentTime => "CURRENT_TIME()".into(),
            DefaultValue::NewUuid => "UUID_STRING()".into(),
            DefaultValue::NextVal(_) | DefaultValue::Expr(_) => return None,
        })
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            on_delete: ALL_ACTIONS,
            on_update: ALL_ACTIONS,
            // Unique indexes become UNIQUE constraints (see finalize).
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 255,
            case: IdentCase::Upper,
        }
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        let name = t.name.clone();
        t.indexes.retain(|ix| {
            if ix.unique {
                report.push(Severity::Info, IssueCode::IndexChanged, &name, Some(&ix.name), "Índice único como restricción UNIQUE, que Snowflake no hace cumplir.");
            } else {
                report.push(Severity::Dropped, IssueCode::IndexDropped, &name, Some(&ix.name), "Snowflake no tiene índices (usa micro-particiones y clustering).");
            }
            ix.unique
        });
        not_enforced(t, report, "Snowflake");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::logical_of;
    use crate::parse::parse;
    use dbine_driver::IndexDef;

    fn lt(s: &str) -> L {
        logical_of(&Snowflake, &parse(s))
    }

    #[test]
    fn parses_catalog_spellings() {
        assert_eq!(lt("NUMBER(38,0)"), L::Decimal { precision: Some(38), scale: Some(0) });
        assert_eq!(lt("NUMBER(10,0)"), L::int(8));
        assert_eq!(lt("NUMBER(9,0)"), L::int(4));
        assert_eq!(lt("NUMBER(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(lt("FLOAT"), L::Float { bytes: 8 });
        assert_eq!(lt("TEXT(16777216)"), L::Text { unicode: true });
        assert_eq!(lt("TEXT(20)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(lt("BINARY(8388608)"), L::Blob);
        assert_eq!(lt("BINARY(16)"), L::Varbinary { len: Some(16) });
        assert_eq!(lt("BOOLEAN"), L::Bool);
        assert_eq!(lt("TIME"), L::Time { precision: Some(9), tz: false });
        assert_eq!(lt("TIMESTAMP_NTZ"), L::Timestamp { precision: Some(9), tz: false });
        assert_eq!(lt("TIMESTAMP_LTZ(3)"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(lt("TIMESTAMP_TZ"), L::Timestamp { precision: Some(9), tz: true });
        assert_eq!(lt("VARIANT"), L::Json { binary: true });
        assert_eq!(lt("OBJECT"), L::Json { binary: true });
        assert_eq!(lt("ARRAY"), L::Json { binary: true });
        assert_eq!(lt("ARRAY(NUMBER(10,0))"), L::Array { of: Box::new(L::int(8)) });
        assert!(matches!(lt("GEOGRAPHY"), L::Geometry { geography: true, .. }));
        assert!(matches!(lt("VECTOR(FLOAT, 256)"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| Snowflake.render_type(&t);
        assert_eq!(r(L::Bool).native, "BOOLEAN");
        assert_eq!(r(L::int(4)).native, "NUMBER(10, 0)");
        assert_eq!(r(L::int(8)).native, "NUMBER(19, 0)");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }).native, "NUMBER(20, 0)");
        assert_eq!(r(L::int(16)).notes[0].code, IssueCode::RangeLoss);
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }).native, "NUMBER(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(65), scale: Some(30) }).native, "NUMBER(38, 30)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }).native, "NUMBER(38, 10)");
        assert_eq!(r(L::Float { bytes: 4 }).native, "FLOAT");
        assert_eq!(r(L::Money).native, "NUMBER(19, 4)");
        assert_eq!(r(L::Char { len: Some(3), unicode: false }).native, "CHAR(3)");
        assert_eq!(r(L::Varchar { len: Some(30), unicode: true }).native, "VARCHAR(30)");
        assert_eq!(r(L::Text { unicode: true }).native, "VARCHAR");
        assert_eq!(r(L::Binary { len: Some(16) }).native, "BINARY(16)");
        assert_eq!(r(L::Blob).native, "BINARY");
        assert_eq!(r(L::Bit { len: Some(8) }).native, "BINARY(1)");
        assert_eq!(r(L::Date).native, "DATE");
        assert_eq!(r(L::Time { precision: Some(3), tz: false }).native, "TIME(3)");
        assert_eq!(r(L::Timestamp { precision: Some(6), tz: true }).native, "TIMESTAMP_TZ(6)");
        assert_eq!(r(L::Timestamp { precision: None, tz: false }).native, "TIMESTAMP_NTZ");
        assert_eq!(r(L::Interval).native, "VARCHAR(64)");
        assert_eq!(r(L::Year).native, "NUMBER(4, 0)");
        assert_eq!(r(L::Uuid).native, "VARCHAR(36)");
        assert_eq!(r(L::Json { binary: true }).native, "VARIANT");
        assert_eq!(r(L::Xml).native, "VARCHAR");
        assert_eq!(r(L::Enum { values: vec!["abc".into()] }).native, "VARCHAR(3)");
        assert_eq!(r(L::Set { values: vec!["a".into()] }).native, "ARRAY");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }).native, "ARRAY");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }).native, "OBJECT");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: true }).native, "GEOGRAPHY");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }).native, "GEOMETRY");
        assert_eq!(r(L::Inet).native, "VARCHAR(45)");
        assert_eq!(r(L::MacAddr).native, "VARCHAR(17)");
        assert_eq!(r(L::RowVersion).native, "BINARY(8)");
    }

    #[test]
    fn defaults_and_finalize() {
        let d = Snowflake;
        assert_eq!(d.render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: true }).as_deref(), Some("CURRENT_TIMESTAMP()"));
        assert_eq!(d.render_default(&DefaultValue::NewUuid, &L::Uuid).as_deref(), Some("UUID_STRING()"));
        assert_eq!(d.render_default(&DefaultValue::Text("a\\b'c".into()), &L::Text { unicode: true }).as_deref(), Some("'a\\\\b''c'"));
        let mut t = TableSchema {
            name: "T".into(),
            indexes: vec![
                IndexDef { name: "U".into(), columns: vec!["A".into()], unique: true, ..Default::default() },
                IndexDef { name: "I".into(), columns: vec!["A".into()], ..Default::default() },
            ],
            ..Default::default()
        };
        let mut rep = Report::default();
        d.finalize(&mut t, &mut rep);
        assert_eq!(t.indexes.len(), 1);
        assert!(rep.issues.iter().any(|i| i.code == IssueCode::IndexDropped));
    }
}
