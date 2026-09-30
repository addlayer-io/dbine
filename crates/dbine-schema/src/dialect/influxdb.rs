//! InfluxDB 1.x (InfluxQL), 2.x (Flux) and 3.x (SQL over Arrow). A
//! measurement is created by writing points to it: there is no CREATE with
//! columns, so InfluxDB reads as a source only. Its columns are the time,
//! tags (strings, the series key) and fields (float, integer, unsigned,
//! string, boolean).
//!
//! Also home of [`source_only`], the report of the engines schema
//! conversion can read from but not create tables in.

use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct InfluxDb;

const WHY: &str = "InfluxDB no tiene DDL de measurements; se crean al escribir puntos (line protocol).";

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: InfluxDb = InfluxDb;
    matches!(driver_id, "influxdb1" | "influxdb" | "influxdb3").then_some(&D as &dyn Dialect)
}

/// A target whose driver can't create the table: the table is reported as
/// left out (the converted definition stays, for reference).
pub(crate) fn source_only(t: &TableSchema, report: &mut Report, why: &str) {
    report.push(Severity::Dropped, IssueCode::OptionDropped, &t.name, None, format!("No se puede crear la tabla en el destino: {why}"));
}

impl Dialect for InfluxDb {
    fn id(&self) -> &'static str {
        "influxdb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        match t.name.as_str() {
            // InfluxQL / Flux: `time`, `tag` and field types.
            "time" => L::Timestamp { precision: Some(9), tz: true },
            "tag" | "string" | "utf8" | "largeutf8" | "utf8view" => L::Text { unicode: true },
            // Arrow (3.x): tags are dictionary-encoded strings.
            "dictionary" => L::Text { unicode: true },
            "float" | "float64" => L::Float { bytes: 8 },
            "integer" | "int64" => L::int(8),
            // `unsigned` alone parses as a modifier with no name.
            "unsigned" | "uint64" => L::Int { bytes: 8, unsigned: true },
            "" if t.unsigned => L::Int { bytes: 8, unsigned: true },
            "boolean" => L::Bool,
            "timestamp" => L::Timestamp { precision: Some(9), tz: true },
            // 2.x reports fields without their type.
            _ => L::Other { native: t.raw.clone() },
        }
    }

    /// Line-protocol field types, for reference (no DDL uses them).
    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let string = |why: &str| Rendered::exact("string").with(Warning, TypeApproximated, why.to_string());
        match t {
            L::Bool => Rendered::exact("boolean"),
            L::Int { bytes: 8, unsigned: true } => Rendered::exact("unsigned"),
            L::Int { bytes, unsigned } if L::signed_bytes_for(*bytes, *unsigned) <= 8 => Rendered::exact("integer"),
            L::Int { .. } => string("Entero de 16 bytes como texto."),
            L::Float { .. } => Rendered::exact("float"),
            L::Decimal { .. } | L::Money => Rendered::exact("float").with(Loss, PrecisionLoss, "InfluxDB no tiene decimales exactos: se guardan como float."),
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } | L::Uuid | L::Enum { .. } => Rendered::exact("string"),
            L::Timestamp { .. } => Rendered::exact("time"),
            L::Other { native } => Rendered::exact(native.clone()),
            _ => string("Se guarda como texto."),
        }
    }

    fn render_default(&self, _d: &DefaultValue, _ty: &L) -> Option<String> {
        None
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
            defaults: false,
            nullability: false,
            comments: false,
            max_identifier: 255,
            case: IdentCase::Preserve,
        }
    }

    fn target_refusal(&self, _: &str) -> Option<&'static str> {
        Some(WHY)
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        source_only(t, report, WHY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn p(s: &str) -> L {
        crate::convert::logical_of(&InfluxDb, &parse(s))
    }

    #[test]
    fn parses_every_version() {
        assert_eq!(p("time"), L::Timestamp { precision: Some(9), tz: true });
        assert_eq!(p("tag"), L::Text { unicode: true });
        assert_eq!(p("float"), L::Float { bytes: 8 });
        assert_eq!(p("integer"), L::int(8));
        assert_eq!(p("unsigned"), L::Int { bytes: 8, unsigned: true });
        assert_eq!(p("string"), L::Text { unicode: true });
        assert_eq!(p("boolean"), L::Bool);
        assert_eq!(p("Timestamp(Nanosecond, None)"), L::Timestamp { precision: Some(9), tz: true });
        assert_eq!(p("Dictionary(Int32, Utf8)"), L::Text { unicode: true });
        assert_eq!(p("Float64"), L::Float { bytes: 8 });
        assert_eq!(p("Int64"), L::int(8));
        assert_eq!(p("UInt64"), L::Int { bytes: 8, unsigned: true });
        assert_eq!(p("Utf8"), L::Text { unicode: true });
        assert_eq!(p("Boolean"), L::Bool);
        assert!(matches!(p("field"), L::Other { .. }));
    }

    #[test]
    fn renders_and_reports_source_only() {
        assert_eq!(InfluxDb.render_type(&L::int(4)).native, "integer");
        assert_eq!(InfluxDb.render_type(&L::Int { bytes: 8, unsigned: true }).native, "unsigned");
        assert_eq!(InfluxDb.render_type(&L::Json { binary: true }).native, "string");
        let mut t = TableSchema { name: "m".into(), ..Default::default() };
        let mut rep = Report::default();
        InfluxDb.finalize(&mut t, &mut rep);
        assert_eq!(rep.issues[0].severity, Severity::Dropped);
    }
}
