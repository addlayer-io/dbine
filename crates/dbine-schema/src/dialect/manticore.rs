//! Manticore Search real-time tables. A handful of types: full-text
//! `text` fields, `string` attributes, 32-bit unsigned `integer`, signed
//! `bigint`, 32-bit `float`, `bool`, `timestamp` (unsigned 32-bit Unix
//! seconds), `json`, multi-value integer sets and float vectors. Every
//! table has an implicit `id bigint` document key; there is no NULL,
//! no defaults, no keys, no indexes besides the full-text ones.

use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct Manticore;

const SET_NOTE: &str = "Arreglo de enteros como atributo multivalor: Manticore lo guarda ordenado y sin repetidos.";

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Manticore = Manticore;
    (driver_id == "manticore").then_some(&D as &dyn Dialect)
}

impl Dialect for Manticore {
    fn id(&self) -> &'static str {
        "manticore"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        match t.name.as_str() {
            "bool" => L::Bool,
            "uint" | "integer" | "int" => L::Int { bytes: 4, unsigned: true },
            "bigint" => L::int(8),
            "float" => L::Float { bytes: 4 },
            // Full-text field (indexed and stored) or plain string attribute.
            "text" | "field" => L::Text { unicode: true },
            "string" => L::Text { unicode: true },
            // Unix seconds, unsigned 32-bit.
            "timestamp" => L::Timestamp { precision: Some(0), tz: true },
            "json" => L::Json { binary: false },
            "mva" | "multi" => L::Array { of: Box::new(L::Int { bytes: 4, unsigned: true }) },
            "mva64" | "multi64" => L::Array { of: Box::new(L::int(8)) },
            "float_vector" => L::Array { of: Box::new(L::Float { bytes: 4 }) },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let string = |why: &str| Rendered::exact("string").with(Warning, TypeApproximated, why.to_string());
        match t {
            L::Bool => Rendered::exact("bool"),
            // `integer` is unsigned 32-bit; signed values need bigint.
            L::Int { bytes, unsigned: true } if *bytes <= 4 => Rendered::exact("integer"),
            L::Int { bytes, .. } if *bytes <= 8 => {
                let r = Rendered::exact("bigint");
                if matches!(t, L::Int { bytes: 8, unsigned: true }) {
                    r.with(Loss, RangeLoss, "Manticore no tiene enteros sin signo de 8 bytes: los valores mayores a 2^63 no entran en bigint.")
                } else {
                    r
                }
            }
            L::Int { .. } => string("Entero de 16 bytes como texto."),
            L::Decimal { .. } | L::Money => Rendered::exact("float")
                .with(Loss, PrecisionLoss, "Manticore solo tiene float de 32 bits (unos 7 dígitos): los decimales se redondean."),
            L::Float { bytes: 4 } => Rendered::exact("float"),
            L::Float { .. } => Rendered::exact("float").with(Loss, PrecisionLoss, "Manticore solo tiene float de 32 bits: el doble se redondea a unos 7 dígitos."),
            L::Char { .. } | L::Varchar { .. } => Rendered::exact("string"),
            L::Text { .. } => Rendered::exact("text").with(Info, TypeChanged, "Texto largo como campo de texto completo (indexado y guardado)."),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => string("Manticore no tiene binarios: los bytes se guardan como texto."),
            L::Bit { len } => match len {
                Some(n) if *n <= 63 => Rendered::exact("bigint").with(Info, TypeChanged, "Cadena de bits como entero."),
                _ => string("Cadena de bits como texto."),
            },
            L::Date => Rendered::exact("timestamp")
                .with(Loss, RangeLoss, "La fecha se guarda como segundos Unix de 32 bits sin signo: solo entre 1970 y 2106."),
            L::Time { .. } => string("Manticore no tiene horas: queda como texto."),
            L::Timestamp { precision, tz } => {
                let mut r = Rendered::exact("timestamp")
                    .with(Loss, RangeLoss, "timestamp de Manticore son segundos Unix de 32 bits sin signo: solo entre 1970 y 2106.");
                if precision.is_none_or(|p| p > 0) {
                    r = r.with(Loss, PrecisionLoss, "Manticore guarda segundos enteros: se pierden las fracciones.");
                }
                if !tz {
                    r = r.with(Info, TimeZoneLoss, "Los valores sin zona se interpretan como UTC.");
                }
                r
            }
            L::Interval => string("Intervalo como texto."),
            L::Year => Rendered::exact("integer").with(Info, TypeChanged, "Año como entero."),
            L::Uuid => Rendered::exact("string").with(Info, TypeChanged, "UUID como texto."),
            L::Json { .. } => Rendered::exact("json"),
            L::Xml => string("XML como texto."),
            L::Enum { values } => string(&format!("Manticore no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => string(&format!("Conjunto como texto. Valores: {}.", values.join(", "))),
            L::Array { of } => match of.as_ref() {
                // multi is unsigned 32-bit; multi64 signed 64-bit.
                L::Int { bytes, unsigned: true } if *bytes <= 4 => Rendered::exact("multi").with(Info, TypeChanged, SET_NOTE),
                L::Int { bytes, .. } if *bytes <= 8 => Rendered::exact("multi64").with(Info, TypeChanged, SET_NOTE),
                L::Float { .. } => Rendered::exact("float_vector"),
                _ => Rendered::exact("json").with(Warning, TypeApproximated, "Arreglo como JSON."),
            },
            L::Map { .. } => Rendered::exact("json").with(Warning, TypeApproximated, "Mapa como JSON."),
            L::Geometry { .. } => string("Dato espacial como texto (WKT)."),
            L::Inet => Rendered::exact("string").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("string").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("bigint").with(Warning, TypeApproximated, "Manticore no tiene versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    /// No defaults in Manticore.
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
            max_identifier: 64,
            // Names are case-insensitive and kept in lower case.
            case: IdentCase::Lower,
        }
    }

    /// The document key is the implicit `id bigint`: a source column named
    /// `id` of another type is renamed, and a key on other columns can't be
    /// kept.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        use IssueCode::*;
        use Severity::*;
        let table = t.name.clone();
        let pk = t.primary_key.take().map(|k| k.columns).unwrap_or_default();
        let taken: Vec<String> = t.columns.iter().map(|c| c.name.to_ascii_lowercase()).collect();
        for c in t.columns.iter_mut().filter(|c| c.name.eq_ignore_ascii_case("id")) {
            if c.data_type.eq_ignore_ascii_case("bigint") {
                c.name = "id".into();
                continue;
            }
            let new = (0..).map(|i| if i == 0 { "id_origen".to_string() } else { format!("id_origen_{i}") }).find(|n| !taken.contains(n)).unwrap();
            report.push(Warning, IdentifierRenamed, &table, Some(&c.name), format!(
                "En Manticore «id» es la clave del documento (bigint): la columna de tipo {} pasa a llamarse «{new}».", c.data_type
            ));
            c.name = new;
        }
        let is_id = pk.len() == 1 && pk[0].eq_ignore_ascii_case("id") && t.columns.iter().any(|c| c.name == "id");
        if !pk.is_empty() && !is_id {
            report.push(Warning, PrimaryKeyDropped, &table, Some(&pk.join(", ")), "Manticore identifica los documentos por «id»: la clave primaria de origen no se controla.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use dbine_driver::{ColumnDef, KeyDef};

    fn p(s: &str) -> L {
        crate::convert::logical_of(&Manticore, &parse(s))
    }

    #[test]
    fn parses_describe_types() {
        assert_eq!(p("bigint"), L::int(8));
        assert_eq!(p("uint"), L::Int { bytes: 4, unsigned: true });
        assert_eq!(p("float"), L::Float { bytes: 4 });
        assert_eq!(p("bool"), L::Bool);
        assert_eq!(p("text"), L::Text { unicode: true });
        assert_eq!(p("string"), L::Text { unicode: true });
        assert_eq!(p("timestamp"), L::Timestamp { precision: Some(0), tz: true });
        assert_eq!(p("json"), L::Json { binary: false });
        assert_eq!(p("mva"), L::Array { of: Box::new(L::Int { bytes: 4, unsigned: true }) });
        assert_eq!(p("mva64"), L::Array { of: Box::new(L::int(8)) });
        assert_eq!(p("float_vector"), L::Array { of: Box::new(L::Float { bytes: 4 }) });
        assert!(matches!(p("tokencount"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| Manticore.render_type(&t).native;
        assert_eq!(r(L::Bool), "bool");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "integer");
        assert_eq!(r(L::int(4)), "bigint");
        assert_eq!(r(L::int(2)), "bigint");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "bigint");
        assert_eq!(r(L::int(16)), "string");
        assert_eq!(r(L::Decimal { precision: Some(10), scale: Some(2) }), "float");
        assert_eq!(r(L::Float { bytes: 8 }), "float");
        assert_eq!(r(L::Money), "float");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }), "string");
        assert_eq!(r(L::Varchar { len: Some(3), unicode: true }), "string");
        assert_eq!(r(L::Text { unicode: true }), "text");
        assert_eq!(r(L::Blob), "string");
        assert_eq!(r(L::Binary { len: Some(2) }), "string");
        assert_eq!(r(L::Varbinary { len: Some(2) }), "string");
        assert_eq!(r(L::Bit { len: Some(8) }), "bigint");
        assert_eq!(r(L::Date), "timestamp");
        assert_eq!(r(L::Time { precision: None, tz: false }), "string");
        assert_eq!(r(L::Timestamp { precision: Some(6), tz: true }), "timestamp");
        assert_eq!(r(L::Interval), "string");
        assert_eq!(r(L::Year), "integer");
        assert_eq!(r(L::Uuid), "string");
        assert_eq!(r(L::Json { binary: true }), "json");
        assert_eq!(r(L::Xml), "string");
        assert_eq!(r(L::Enum { values: vec![] }), "string");
        assert_eq!(r(L::Set { values: vec![] }), "string");
        assert_eq!(r(L::Array { of: Box::new(L::Int { bytes: 4, unsigned: true }) }), "multi");
        assert_eq!(r(L::Array { of: Box::new(L::int(2)) }), "multi64");
        assert_eq!(r(L::Array { of: Box::new(L::int(8)) }), "multi64");
        assert_eq!(r(L::Array { of: Box::new(L::Float { bytes: 4 }) }), "float_vector");
        assert_eq!(r(L::Array { of: Box::new(L::Text { unicode: true }) }), "json");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "json");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "string");
        assert_eq!(r(L::Inet), "string");
        assert_eq!(r(L::MacAddr), "string");
        assert_eq!(r(L::RowVersion), "bigint");
        assert_eq!(Manticore.render_default(&DefaultValue::Number("1".into()), &L::int(4)), None);
    }

    #[test]
    fn finalize_renames_a_non_bigint_id() {
        let mut t = TableSchema {
            name: "t".into(),
            columns: vec![
                ColumnDef { name: "ID".into(), data_type: "string".into(), ..Default::default() },
                ColumnDef { name: "n".into(), data_type: "bigint".into(), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["ID".into()] }),
            ..Default::default()
        };
        let mut rep = Report::default();
        Manticore.finalize(&mut t, &mut rep);
        assert_eq!(t.columns[0].name, "id_origen");
        assert!(t.primary_key.is_none());
        assert!(rep.issues.iter().any(|i| i.code == IssueCode::PrimaryKeyDropped));
    }
}
