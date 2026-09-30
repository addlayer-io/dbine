//! Apache Solr (managed schema, `_default` configset).
//!
//! Fields come from the Schema API: the field type (`pint`, `string`,
//! `text_general`, `pdate`…), `required`, `default`, and multi-valued
//! fields as `type[]` (`columns`) or the plural types (`pints`,
//! `strings`…). The same `[]` suffix makes the designer write
//! `multiValued: true`.
//!
//! From SQL, strings follow the same rule as Elasticsearch: bounded ones
//! up to 256 characters are `string` (exact match, sortable), longer or
//! unbounded ones `text_general` (full-text). Solr has no exact decimal:
//! `pdouble`. The configset's uniqueKey is `id`, a `string` that the
//! Schema API can't change, so the primary key only survives as `id`; a
//! key with another name is reported (the copy has to fill `id`).

use super::elasticsearch::EXACT_TEXT_MAX;
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{KeyDef, TableSchema};

pub struct Solr;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Solr = Solr;
    (driver_id == "solr").then_some(&D as &dyn Dialect)
}

fn field_type(name: &str) -> L {
    let one = |n: &str| -> L {
        match n {
            "string" | "text_general" | "text_gen_sort" | "text_ws" | "lowercase" | "phonetic_en" | "descendent_path" | "ancestor_path" => {
                L::Text { unicode: true }
            }
            n if n.starts_with("text_") => L::Text { unicode: true },
            "pint" | "int" | "tint" => L::int(4),
            "plong" | "long" | "tlong" => L::int(8),
            "pfloat" | "float" | "tfloat" | "rank" => L::Float { bytes: 4 },
            "pdouble" | "double" | "tdouble" => L::Float { bytes: 8 },
            "boolean" => L::Bool,
            "pdate" | "date" | "tdate" => L::Timestamp { precision: Some(3), tz: true },
            "binary" => L::Blob,
            "uuid" => L::Uuid,
            "currency" => L::Money,
            "location" | "latlon" | "point" => L::Geometry { kind: Some("point".into()), srid: Some(4326), geography: true },
            "location_rpt" | "bbox" => L::Geometry { kind: None, srid: Some(4326), geography: true },
            _ => L::Other { native: n.to_string() },
        }
    };
    // Multi-valued types of the `_default` configset.
    let plural = match name {
        "strings" => Some("string"),
        "booleans" => Some("boolean"),
        "pints" => Some("pint"),
        "plongs" => Some("plong"),
        "pfloats" => Some("pfloat"),
        "pdoubles" => Some("pdouble"),
        "pdates" => Some("pdate"),
        _ => None,
    };
    match plural {
        Some(p) => L::Array { of: Box::new(one(p)) },
        None => one(name),
    }
}

impl Dialect for Solr {
    fn id(&self) -> &'static str {
        "solr"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        match field_type(&t.name) {
            L::Other { .. } => L::Other { native: t.raw.clone() },
            l => l,
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("boolean"),
            L::Int { bytes, unsigned } => match L::signed_bytes_for(*bytes, *unsigned) {
                1..=4 => Rendered::exact("pint"),
                8 => Rendered::exact("plong"),
                _ => Rendered::exact("pdouble").with(Loss, RangeLoss, "Entero de más de 8 bytes como pdouble: pierde precisión pasados los 15 dígitos."),
            },
            L::Decimal { precision: Some(p), .. } if *p <= 15 => {
                Rendered::exact("pdouble").with(Info, TypeChanged, "Solr no tiene decimales exactos: pdouble, exacto hasta 15 dígitos.")
            }
            L::Decimal { .. } => Rendered::exact("pdouble").with(Loss, PrecisionLoss, "Solr no tiene decimales exactos: pdouble pierde precisión pasados los 15 dígitos."),
            L::Float { bytes: 4 } => Rendered::exact("pfloat"),
            L::Float { .. } => Rendered::exact("pdouble"),
            L::Money => Rendered::exact("pdouble").with(Info, TypeChanged, "Moneda como pdouble."),
            L::Char { len, .. } | L::Varchar { len: len @ Some(_), .. } if len.unwrap_or(1) <= EXACT_TEXT_MAX => Rendered::exact("string"),
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } => Rendered::exact("text_general"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("binary"),
            L::Bit { .. } => Rendered::exact("string").with(Warning, TypeApproximated, "Cadena de bits como texto de 0 y 1."),
            L::Date => Rendered::exact("pdate").with(Info, TypeChanged, "Fecha como pdate, a las 00:00 UTC."),
            L::Time { .. } => Rendered::exact("string").with(Warning, TypeApproximated, "Solr no tiene hora del día: queda como texto."),
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact("pdate");
                let r = match precision {
                    Some(p) if *p > 3 => r.with(Loss, PrecisionLoss, format!("pdate guarda milisegundos: se pierden {} decimales de segundo.", p - 3)),
                    _ => r,
                };
                if *tz {
                    r
                } else {
                    r.with(Warning, TimeZoneLoss, "pdate es un instante en UTC: la fecha y hora sin zona se toma como UTC.")
                }
            }
            L::Interval => Rendered::exact("string").with(Warning, TypeApproximated, "Solr no tiene intervalos: queda como texto."),
            L::Year => Rendered::exact("pint").with(Info, TypeChanged, "Año como pint."),
            L::Uuid => Rendered::exact("string"),
            L::Json { .. } | L::Map { .. } => Rendered::exact("string").with(Warning, TypeApproximated, "Solr no tiene campos JSON: queda como texto."),
            L::Xml => Rendered::exact("text_general").with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } => Rendered::exact("string").with(Info, TypeApproximated, format!("Enumerado como texto. Valores: {}.", values.join(", "))),
            L::Set { .. } => Rendered::exact("string[]").with(Info, TypeChanged, "Conjunto como campo multivaluado."),
            L::Array { of } if matches!(**of, L::Array { .. }) => {
                Rendered::exact("string[]").with(Warning, TypeApproximated, "Solr no tiene arreglos anidados: cada elemento queda como texto.")
            }
            L::Array { of } => {
                let inner = self.render_type(of);
                Rendered { native: format!("{}[]", inner.native), notes: inner.notes }
            }
            L::Geometry { kind: Some(k), .. } if k == "point" => Rendered::exact("location").with(Warning, TypeApproximated, "Punto como location: los valores van como \"lat,lon\"."),
            L::Geometry { .. } => Rendered::exact("location_rpt").with(Warning, TypeApproximated, "Dato espacial como location_rpt: los valores van como WKT."),
            L::Inet | L::MacAddr => Rendered::exact("string"),
            L::RowVersion => Rendered::exact("binary").with(Warning, TypeApproximated, "Solr no tiene versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    /// Solr takes the default as the value's text; dates can be `NOW`.
    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => s.clone(),
            DefaultValue::Bool(b) => b.to_string(),
            DefaultValue::CurrentTimestamp | DefaultValue::CurrentDate if matches!(ty, L::Date | L::Timestamp { .. }) => "NOW".into(),
            _ => return None,
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
            comments: false,
            max_identifier: 255,
            case: IdentCase::Preserve,
        }
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        let source_name = t.name.clone();
        t.kind = dbine_driver::kinds::COLLECTION.into();
        let name: String = t.name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' }).collect();
        let name = name.trim_start_matches('-').to_string();
        if name != t.name {
            report.push(Severity::Info, IssueCode::IdentifierRenamed, &source_name, Some(&source_name), format!("Nombre de colección válido para Solr: «{name}»."));
            t.name = name;
        }
        let key = t.primary_key.take().map(|k| k.columns).unwrap_or_default();
        if key != ["id"] {
            let msg = if key.is_empty() {
                "La colección se identifica por «id» (uniqueKey del configset): al copiar hay que llenarlo.".to_string()
            } else {
                format!("La colección se identifica por «id» (uniqueKey del configset): al copiar hay que llenarlo con la clave ({}).", key.join(", "))
            };
            report.push(Severity::Warning, IssueCode::PrimaryKeyDropped, &source_name, None, msg);
        }
        if let Some(c) = t.columns.iter_mut().find(|c| c.name == "id") {
            if c.data_type != "string" {
                report.push(
                    Severity::Warning,
                    IssueCode::TypeChanged,
                    &source_name,
                    Some("id"),
                    format!("El uniqueKey «id» es de tipo string en el configset: queda string (era {}).", c.data_type),
                );
                c.data_type = "string".into();
            }
        }
        t.primary_key = Some(KeyDef { name: None, columns: vec!["id".into()] });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn parses_field_types() {
        let p = |s: &str| Solr.parse_type(&parse(s));
        assert_eq!(p("string"), L::Text { unicode: true });
        assert_eq!(p("text_en"), L::Text { unicode: true });
        assert_eq!(p("pint"), L::int(4));
        assert_eq!(p("plong"), L::int(8));
        assert_eq!(p("pdouble"), L::Float { bytes: 8 });
        assert_eq!(p("pdate"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(p("pints"), L::Array { of: Box::new(L::int(4)) });
        assert_eq!(p("booleans"), L::Array { of: Box::new(L::Bool) });
        assert_eq!(p("location"), L::Geometry { kind: Some("point".into()), srid: Some(4326), geography: true });
        assert!(matches!(p("my_custom"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| Solr.render_type(&t).native;
        assert_eq!(r(L::Bool), "boolean");
        assert_eq!(r(L::int(4)), "pint");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "plong");
        assert_eq!(r(L::int(16)), "pdouble");
        assert_eq!(r(L::Decimal { precision: Some(10), scale: Some(2) }), "pdouble");
        assert_eq!(r(L::Float { bytes: 4 }), "pfloat");
        assert_eq!(r(L::Money), "pdouble");
        assert_eq!(r(L::Varchar { len: Some(100), unicode: true }), "string");
        assert_eq!(r(L::Varchar { len: Some(1000), unicode: true }), "text_general");
        assert_eq!(r(L::Text { unicode: true }), "text_general");
        assert_eq!(r(L::Blob), "binary");
        assert_eq!(r(L::Bit { len: None }), "string");
        assert_eq!(r(L::Date), "pdate");
        assert_eq!(r(L::Time { precision: None, tz: false }), "string");
        assert_eq!(r(L::Timestamp { precision: None, tz: true }), "pdate");
        assert_eq!(r(L::Interval), "string");
        assert_eq!(r(L::Year), "pint");
        assert_eq!(r(L::Uuid), "string");
        assert_eq!(r(L::Json { binary: true }), "string");
        assert_eq!(r(L::Xml), "text_general");
        assert_eq!(r(L::Enum { values: vec![] }), "string");
        assert_eq!(r(L::Set { values: vec![] }), "string[]");
        assert_eq!(r(L::Array { of: Box::new(L::int(8)) }), "plong[]");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "string");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "location_rpt");
        assert_eq!(r(L::Inet), "string");
        assert_eq!(r(L::MacAddr), "string");
        assert_eq!(r(L::RowVersion), "binary");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
        assert_eq!(Solr.render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: true }).as_deref(), Some("NOW"));
        assert_eq!(Solr.render_default(&DefaultValue::Text("a".into()), &L::Text { unicode: true }).as_deref(), Some("a"));
        assert_eq!(Solr.render_default(&DefaultValue::NewUuid, &L::Uuid), None);
    }
}
