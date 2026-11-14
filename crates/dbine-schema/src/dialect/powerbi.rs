//! Power BI / Microsoft Fabric semantic models (the `powerbi` driver). A
//! table's columns carry the model's data types: `Int64`, `Decimal` (fixed
//! decimal, four decimals), `Double`, `String`, `DateTime`, `Boolean`,
//! `Binary` and `Variant`; a calculated column says so after its type
//! (`Int64 (calculada)`). DBine treats a model as a read-only target, so it
//! reads as a source only. `Variant` values come to DBine as text.

use super::influxdb::source_only;
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct PowerBi;

const WHY: &str = "DBine no crea tablas en un modelo semántico de Power BI: es un destino de solo lectura (sus tablas se publican desde Power BI Desktop o Fabric).";

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: PowerBi = PowerBi;
    (driver_id == "powerbi").then_some(&D as &dyn Dialect)
}

impl Dialect for PowerBi {
    fn id(&self) -> &'static str {
        "powerbi"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        match t.name.as_str() {
            "int64" | "integer" | "whole number" => L::int(8),
            // TOM's Decimal is the fixed decimal number (currency).
            "decimal" | "currency" | "fixed decimal number" => L::Decimal {
                precision: Some(19),
                scale: Some(4),
            },
            "double" | "decimal number" => L::Float { bytes: 8 },
            "string" | "text" => L::Text { unicode: true },
            // A double count of days: milliseconds at best.
            "datetime" | "date" => L::Timestamp {
                precision: Some(3),
                tz: false,
            },
            "boolean" | "true/false" => L::Bool,
            "binary" => L::Blob,
            "variant" => L::Text { unicode: true },
            _ => L::Other {
                native: t.raw.clone(),
            },
        }
    }

    /// The model's data types, for reference (no DDL uses them).
    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let text = |why: &str| Rendered::exact("String").with(Warning, TypeApproximated, why);
        match t {
            L::Bool => Rendered::exact("Boolean"),
            L::Int { bytes, unsigned } if L::signed_bytes_for(*bytes, *unsigned) <= 8 => {
                Rendered::exact("Int64")
            }
            L::Int { .. } => Rendered::exact("Double").with(
                Loss,
                PrecisionLoss,
                "Entero de más de 64 bits como Double: los valores grandes se redondean.",
            ),
            L::Decimal { .. } | L::Money => Rendered::exact("Decimal").with(
                Warning,
                PrecisionLoss,
                "Decimal fijo de Power BI: cuatro decimales.",
            ),
            L::Float { .. } => Rendered::exact("Double"),
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } | L::Uuid | L::Enum { .. } => {
                Rendered::exact("String")
            }
            L::Date | L::Timestamp { .. } => Rendered::exact("DateTime"),
            L::Time { .. } => Rendered::exact("DateTime").with(
                Info,
                TypeChanged,
                "Hora como fecha y hora (el día queda en 1899-12-30).",
            ),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("Binary"),
            L::Other { native } => Rendered::exact(native.clone()),
            _ => text("Se guarda como texto."),
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
            max_identifier: 512,
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
        crate::convert::logical_of(lookup("powerbi").unwrap(), &parse(s))
    }

    #[test]
    fn reads_the_models_types() {
        assert_eq!(p("Int64"), L::int(8));
        assert_eq!(p("Int64 (calculada)"), L::int(8));
        assert_eq!(
            p("Decimal"),
            L::Decimal {
                precision: Some(19),
                scale: Some(4)
            }
        );
        assert_eq!(p("Double"), L::Float { bytes: 8 });
        assert_eq!(p("String"), L::Text { unicode: true });
        assert_eq!(
            p("DateTime"),
            L::Timestamp {
                precision: Some(3),
                tz: false
            }
        );
        assert_eq!(p("Boolean"), L::Bool);
        assert_eq!(p("Binary"), L::Blob);
        assert_eq!(p("Variant"), L::Text { unicode: true });
        assert!(matches!(p("Unknown"), L::Other { .. }));
    }

    #[test]
    fn renders_for_reference_and_is_source_only() {
        let d = lookup("powerbi").unwrap();
        let r = |t: L| d.render_type(&t).native;
        assert_eq!(r(L::Bool), "Boolean");
        assert_eq!(r(L::int(4)), "Int64");
        assert_eq!(r(L::Money), "Decimal");
        assert_eq!(r(L::Float { bytes: 4 }), "Double");
        assert_eq!(r(L::Text { unicode: true }), "String");
        assert_eq!(r(L::Date), "DateTime");
        assert_eq!(r(L::Blob), "Binary");
        assert_eq!(r(L::Json { binary: false }), "String");
        assert!(d.target_refusal("powerbi").is_some());
        let mut t = TableSchema {
            name: "Ventas".into(),
            ..Default::default()
        };
        let mut rep = Report::default();
        d.finalize(&mut t, &mut rep);
        assert_eq!(rep.issues[0].severity, Severity::Dropped);
    }
}
