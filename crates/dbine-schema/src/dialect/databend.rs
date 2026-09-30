//! Databend: MySQL protocol, its own type system (Snowflake-like). Text
//! and binary have no length, TIMESTAMP is stored in UTC, JSON is VARIANT,
//! containers are `ARRAY(T)` / `MAP(K, V)` / `TUPLE(…)`. Tables have no
//! primary key, indexes or foreign keys.

use super::postgres::{longest, precision_loss};
use super::starrocks::{capped_decimal, merge, nested, unbounded_decimal, wrap};
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::{quote, DefaultValue};
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct Databend;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Databend = Databend;
    (driver_id == "databend").then_some(&D as &dyn Dialect)
}

impl Dialect for Databend {
    fn id(&self) -> &'static str {
        "databend"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let u = t.unsigned;
        match t.name.as_str() {
            "boolean" | "bool" => L::Bool,
            "tinyint" | "int8" => L::Int { bytes: 1, unsigned: u },
            "smallint" | "int16" => L::Int { bytes: 2, unsigned: u },
            "int" | "integer" | "int32" => L::Int { bytes: 4, unsigned: u },
            "bigint" | "int64" => L::Int { bytes: 8, unsigned: u },
            "uint8" => L::Int { bytes: 1, unsigned: true },
            "uint16" => L::Int { bytes: 2, unsigned: true },
            "uint32" => L::Int { bytes: 4, unsigned: true },
            "uint64" => L::Int { bytes: 8, unsigned: true },
            "decimal" | "numeric" => L::Decimal { precision: p(0).or(Some(18)), scale: p(1).or(Some(0)) },
            "float" | "float32" | "real" => L::Float { bytes: 4 },
            "double" | "float64" | "double precision" => L::Float { bytes: 8 },
            "varchar" | "string" | "text" | "char" | "character varying" => L::Text { unicode: true },
            "binary" | "varbinary" | "blob" | "bytea" => L::Blob,
            "date" => L::Date,
            // Stored as UTC microseconds, shown in the session's zone.
            "timestamp" | "datetime" => L::Timestamp { precision: Some(6), tz: true },
            "interval" => L::Interval,
            "variant" | "json" => L::Json { binary: true },
            "array" => match t.args.as_slice() {
                [of] => L::Array { of: Box::new(nested(self, of)) },
                _ => L::Other { native: t.raw.clone() },
            },
            "map" => match t.args.as_slice() {
                [k, v] => L::Map { key: Box::new(nested(self, k)), value: Box::new(nested(self, v)) },
                _ => L::Other { native: t.raw.clone() },
            },
            "tuple" => L::Json { binary: false },
            "geometry" => L::Geometry { kind: None, srid: None, geography: false },
            "geography" => L::Geometry { kind: None, srid: None, geography: true },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let text = |why: &str| Rendered::exact("VARCHAR").with(Warning, TypeApproximated, why.to_string());
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => {
                let u = if *unsigned { " UNSIGNED" } else { "" };
                match bytes {
                    1 => Rendered::exact(format!("TINYINT{u}")),
                    2 => Rendered::exact(format!("SMALLINT{u}")),
                    3 | 4 => Rendered::exact(format!("INT{u}")),
                    8 => Rendered::exact(format!("BIGINT{u}")),
                    _ => Rendered::exact("DECIMAL(39, 0)").with(Info, TypeChanged, "Entero de 16 bytes como DECIMAL(39, 0)."),
                }
            }
            L::Decimal { precision: Some(p), scale } => capped_decimal(*p, *scale, 76, "Databend"),
            L::Decimal { precision: None, .. } => unbounded_decimal("Databend"),
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(Info, TypeChanged, "Moneda como DECIMAL(19, 4)."),
            L::Char { .. } => Rendered::exact("VARCHAR").with(Info, TypeChanged, "Databend no tiene texto de largo fijo: VARCHAR sin límite ni relleno."),
            L::Varchar { .. } | L::Text { .. } => Rendered::exact("VARCHAR"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("BINARY"),
            L::Bit { len } => match len {
                Some(n) if *n <= 64 => Rendered::exact("BIGINT UNSIGNED").with(Info, TypeChanged, "Cadena de bits como entero."),
                _ => Rendered::exact("BINARY").with(Warning, TypeApproximated, "Cadena de bits larga: se guarda como binario."),
            },
            L::Date => Rendered::exact("DATE"),
            L::Time { tz, .. } => {
                let r = text("Databend no tiene columnas de hora: queda como texto HH:MM:SS.");
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Se pierde la zona horaria de la hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact("TIMESTAMP").with_loss(precision_loss(*precision, 6));
                if *tz {
                    r
                } else {
                    r.with(Info, TimeZoneLoss, "Databend guarda TIMESTAMP en UTC: los valores sin zona se interpretan en la zona de la sesión.")
                }
            }
            L::Interval => text("Intervalo como texto."),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            L::Uuid => Rendered::exact("VARCHAR").with(Info, TypeChanged, "UUID como texto."),
            L::Json { .. } => Rendered::exact("VARIANT"),
            L::Xml => text("XML como texto."),
            L::Enum { values } => text(&format!("Databend no tiene enumerados: queda como texto. Valores: {} (hasta {} caracteres).", values.join(", "), longest(values))),
            L::Set { values } => Rendered::exact("ARRAY(VARCHAR)").with(Warning, TypeApproximated, format!("Conjunto como arreglo de texto. Valores: {}.", values.join(", "))),
            L::Array { of } => wrap(self.render_type(of), |i| format!("ARRAY({i})")),
            L::Map { key, value } => merge(self.render_type(key), self.render_type(value), |k, v| format!("MAP({k}, {v})")),
            L::Geometry { .. } => text("Dato espacial como texto (WKT): GEOMETRY de Databend requiere habilitarlo en el servidor."),
            L::Inet => Rendered::exact("VARCHAR").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("BINARY").with(Warning, TypeApproximated, "Databend no tiene versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::Null => "NULL".into(),
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => quote(s),
            DefaultValue::Bool(b) if matches!(ty, L::Bool) => if *b { "TRUE" } else { "FALSE" }.into(),
            DefaultValue::Bool(b) => if *b { "1" } else { "0" }.into(),
            DefaultValue::CurrentTimestamp => "now()".into(),
            DefaultValue::CurrentDate => "today()".into(),
            DefaultValue::NewUuid => "uuid()".into(),
            DefaultValue::CurrentTime | DefaultValue::NextVal(_) | DefaultValue::Expr(_) => return None,
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
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 255,
            // Unquoted names are folded to lower case.
            case: IdentCase::Lower,
        }
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        if let Some(k) = t.primary_key.take() {
            report.push(
                Severity::Warning,
                IssueCode::PrimaryKeyDropped,
                &t.name,
                Some(&k.columns.join(", ")),
                "Databend no tiene claves primarias: la unicidad no se controla.",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn p(s: &str) -> L {
        crate::convert::logical_of(&Databend, &parse(s))
    }

    #[test]
    fn parses() {
        assert_eq!(p("BOOLEAN"), L::Bool);
        assert_eq!(p("TINYINT UNSIGNED"), L::Int { bytes: 1, unsigned: true });
        assert_eq!(p("Nullable(Int32)"), L::int(4));
        assert_eq!(p("UInt64"), L::Int { bytes: 8, unsigned: true });
        assert_eq!(p("BIGINT"), L::int(8));
        assert_eq!(p("DECIMAL(10, 2)"), L::Decimal { precision: Some(10), scale: Some(2) });
        assert_eq!(p("Float32"), L::Float { bytes: 4 });
        assert_eq!(p("DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(p("VARCHAR"), L::Text { unicode: true });
        assert_eq!(p("String"), L::Text { unicode: true });
        assert_eq!(p("BINARY"), L::Blob);
        assert_eq!(p("DATE"), L::Date);
        assert_eq!(p("TIMESTAMP"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(p("VARIANT"), L::Json { binary: true });
        assert_eq!(p("ARRAY(INT32)"), L::Array { of: Box::new(L::int(4)) });
        assert_eq!(p("MAP(STRING, INT64)"), L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::int(8)) });
        assert_eq!(p("TUPLE(INT, STRING)"), L::Json { binary: false });
        assert_eq!(p("INTERVAL"), L::Interval);
        assert!(matches!(p("BITMAP"), L::Other { .. }));
    }

    #[test]
    fn renders() {
        let r = |t: L| Databend.render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "INT UNSIGNED");
        assert_eq!(r(L::Int { bytes: 3, unsigned: false }), "INT");
        assert_eq!(r(L::int(16)), "DECIMAL(39, 0)");
        assert_eq!(r(L::Decimal { precision: Some(60), scale: Some(4) }), "DECIMAL(60, 4)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "DECIMAL(38, 10)");
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Float { bytes: 8 }), "DOUBLE");
        assert_eq!(r(L::Money), "DECIMAL(19, 4)");
        assert_eq!(r(L::Char { len: Some(2), unicode: true }), "VARCHAR");
        assert_eq!(r(L::Varchar { len: Some(20), unicode: true }), "VARCHAR");
        assert_eq!(r(L::Text { unicode: false }), "VARCHAR");
        assert_eq!(r(L::Binary { len: Some(4) }), "BINARY");
        assert_eq!(r(L::Blob), "BINARY");
        assert_eq!(r(L::Bit { len: Some(3) }), "BIGINT UNSIGNED");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: None, tz: true }), "VARCHAR");
        assert_eq!(r(L::Timestamp { precision: Some(3), tz: true }), "TIMESTAMP");
        assert_eq!(r(L::Interval), "VARCHAR");
        assert_eq!(r(L::Year), "SMALLINT");
        assert_eq!(r(L::Uuid), "VARCHAR");
        assert_eq!(r(L::Json { binary: false }), "VARIANT");
        assert_eq!(r(L::Xml), "VARCHAR");
        assert_eq!(r(L::Enum { values: vec!["a".into()] }), "VARCHAR");
        assert_eq!(r(L::Set { values: vec!["a".into()] }), "ARRAY(VARCHAR)");
        assert_eq!(r(L::Array { of: Box::new(L::int(8)) }), "ARRAY(BIGINT)");
        assert_eq!(r(L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::Bool) }), "MAP(VARCHAR, BOOLEAN)");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "VARCHAR");
        assert_eq!(r(L::Inet), "VARCHAR");
        assert_eq!(r(L::MacAddr), "VARCHAR");
        assert_eq!(r(L::RowVersion), "BINARY");
        let ts = Databend.render_type(&L::Timestamp { precision: Some(9), tz: false });
        assert!(ts.notes.iter().any(|n| n.code == IssueCode::PrecisionLoss));
    }

    #[test]
    fn defaults_and_finalize() {
        let ts = L::Timestamp { precision: None, tz: true };
        assert_eq!(Databend.render_default(&DefaultValue::CurrentTimestamp, &ts).as_deref(), Some("now()"));
        assert_eq!(Databend.render_default(&DefaultValue::NewUuid, &L::Uuid).as_deref(), Some("uuid()"));
        assert_eq!(Databend.render_default(&DefaultValue::Bool(false), &L::Bool).as_deref(), Some("FALSE"));
        let mut t = TableSchema { name: "t".into(), primary_key: Some(dbine_driver::KeyDef { name: None, columns: vec!["id".into()] }), ..Default::default() };
        let mut rep = Report::default();
        Databend.finalize(&mut t, &mut rep);
        assert!(t.primary_key.is_none());
        assert_eq!(rep.issues[0].code, IssueCode::PrimaryKeyDropped);
    }
}
