//! Elasticsearch, OpenSearch and Open Distro: an index with an explicit
//! mapping (`properties`), which `database_schema` reads back as columns
//! with the field type as `data_type` and the mapping parameters as column
//! options (`format`, `scaling_factor`…).
//!
//! Decisions from SQL:
//! - Strings: bounded ones up to [`EXACT_TEXT_MAX`] characters (`char`,
//!   `varchar(n)`, enumerations, UUIDs) are `keyword`: codes, names, emails
//!   and states are filtered, sorted and aggregated by their exact value.
//!   Longer or unbounded text is prose, `text` (analyzed, full-text
//!   searchable).
//! - Exact decimals: `scaled_float` with a scaling factor of 10^scale when
//!   the scaled value fits a long (precision up to 18), which keeps cents
//!   exact; a scale of 0 is a `long`; otherwise `double` (reported).
//! - Dates: `date` (milliseconds), `date_nanos` when the source keeps more;
//!   both with a format that also takes `yyyy-MM-dd HH:mm:ss[.fraction]`,
//!   the way SQL drivers print timestamps.
//! - JSON: `flattened` (Elasticsearch) / `flat_object` (OpenSearch), which
//!   index any structure without growing the mapping; `object` on Open
//!   Distro.
//! - The primary key isn't part of a mapping: documents are identified by
//!   `_id`, which the copy fills from an `_id` column (reported).

use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

/// Longest bounded string that becomes `keyword` (ES's own dynamic mapping
/// keeps a keyword sub-field up to 256 characters).
pub(crate) const EXACT_TEXT_MAX: u32 = 256;

#[derive(Clone, Copy, PartialEq)]
enum Flavor {
    Elastic,
    OpenSearch,
    OpenDistro,
}

pub struct Search {
    flavor: Flavor,
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static ES: Search = Search { flavor: Flavor::Elastic };
    static OS: Search = Search { flavor: Flavor::OpenSearch };
    static OD: Search = Search { flavor: Flavor::OpenDistro };
    match driver_id {
        "elasticsearch" => Some(&ES),
        "opensearch" => Some(&OS),
        "opendistro" => Some(&OD),
        _ => None,
    }
}

/// Date formats: ISO 8601, the space-separated form SQL drivers print
/// (`2024-01-31 13:45:00.123`, with `+00`, `+0000` or `+00:00` zones or
/// none) and epoch milliseconds.
const DATE_FORMAT: &str = "strict_date_optional_time||yyyy-MM-dd HH:mm:ss[.SSSSSSSSS][.SSSSSS][.SSS][XXX][X]||epoch_millis";

fn field_type(name: &str) -> L {
    match name {
        "text" | "match_only_text" | "search_as_you_type" | "completion" | "wildcard" | "keyword" | "constant_keyword" | "version" => {
            L::Text { unicode: true }
        }
        "long" => L::int(8),
        "integer" | "token_count" => L::int(4),
        "short" => L::int(2),
        "byte" => L::int(1),
        "unsigned_long" => L::Int { bytes: 8, unsigned: true },
        "double" => L::Float { bytes: 8 },
        "float" | "half_float" | "rank_feature" => L::Float { bytes: 4 },
        // A long divided by the scaling factor: exact at that scale.
        "scaled_float" => L::Decimal { precision: None, scale: None },
        "boolean" => L::Bool,
        "date" => L::Timestamp { precision: Some(3), tz: true },
        "date_nanos" => L::Timestamp { precision: Some(9), tz: true },
        "ip" => L::Inet,
        "binary" => L::Blob,
        "geo_point" => L::Geometry { kind: Some("point".into()), srid: Some(4326), geography: true },
        "geo_shape" => L::Geometry { kind: None, srid: Some(4326), geography: true },
        "dense_vector" | "knn_vector" => L::Array { of: Box::new(L::Float { bytes: 4 }) },
        "object" | "nested" | "flattened" | "flat_object" | "sparse_vector" | "rank_features" | "histogram" | "percolator"
        | "integer_range" | "long_range" | "float_range" | "double_range" | "date_range" | "ip_range" => L::Json { binary: true },
        _ => L::Other { native: name.to_string() },
    }
}

impl Search {
    fn json_type(&self) -> &'static str {
        match self.flavor {
            Flavor::Elastic => "flattened",
            Flavor::OpenSearch => "flat_object",
            Flavor::OpenDistro => "object",
        }
    }
}

impl Dialect for Search {
    fn id(&self) -> &'static str {
        "elasticsearch"
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
            L::Int { bytes, unsigned: false } => Rendered::exact(match bytes {
                1 => "byte",
                2 => "short",
                3 | 4 => "integer",
                8 => "long",
                _ => return Rendered::exact("double").with(Loss, RangeLoss, "Entero de 16 bytes como double: pierde precisión pasados los 15 dígitos."),
            }),
            L::Int { bytes, unsigned: true } => match bytes {
                1 => Rendered::exact("short"),
                2 | 3 => Rendered::exact("integer"),
                4 => Rendered::exact("long"),
                8 if self.flavor != Flavor::OpenDistro => Rendered::exact("unsigned_long"),
                _ => Rendered::exact("double").with(Loss, RangeLoss, "Entero sin signo grande como double: pierde precisión pasados los 15 dígitos."),
            },
            L::Decimal { precision: Some(p), scale: Some(0) } if *p <= 18 => Rendered::exact("long"),
            L::Decimal { precision: Some(p), scale: Some(s) } if *p <= 18 => {
                Rendered::exact(format!("scaled_float({})", 10u64.pow(*s))).with(Info, TypeChanged, format!("Decimal como scaled_float con factor 10^{s}: exacto hasta {s} decimales."))
            }
            L::Decimal { .. } => Rendered::exact("double").with(Loss, PrecisionLoss, "Decimal sin precisión fija o de más de 18 dígitos como double: pierde precisión pasados los 15 dígitos."),
            L::Float { bytes: 4 } => Rendered::exact("float"),
            L::Float { .. } => Rendered::exact("double"),
            L::Money => Rendered::exact("scaled_float(10000)").with(Info, TypeChanged, "Moneda como scaled_float con 4 decimales."),
            L::Char { len, .. } | L::Varchar { len: len @ Some(_), .. } if len.unwrap_or(1) <= EXACT_TEXT_MAX => Rendered::exact("keyword"),
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } => Rendered::exact("text"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("binary").with(Info, TypeChanged, "binary guarda base64 y no se puede buscar."),
            L::Bit { .. } => Rendered::exact("keyword").with(Warning, TypeApproximated, "Cadena de bits como texto de 0 y 1."),
            L::Date => Rendered::exact("date"),
            L::Time { .. } => Rendered::exact("keyword").with(Warning, TypeApproximated, "No hay tipo para la hora del día: queda como texto."),
            L::Timestamp { precision, tz } => {
                let r = if precision.is_some_and(|p| p > 3) { Rendered::exact("date_nanos") } else { Rendered::exact("date") };
                if *tz {
                    r
                } else {
                    r.with(Warning, TimeZoneLoss, "Las fechas se guardan en UTC: la fecha y hora sin zona se toma como UTC.")
                }
            }
            L::Interval => Rendered::exact("keyword").with(Warning, TypeApproximated, "No hay intervalos: queda como texto."),
            L::Year => Rendered::exact("short").with(Info, TypeChanged, "Año como short."),
            L::Uuid => Rendered::exact("keyword"),
            L::Json { .. } | L::Map { .. } => {
                let ty = self.json_type();
                let r = Rendered::exact(ty);
                if ty == "object" {
                    r.with(Info, TypeChanged, "JSON como object: sus campos se agregan al mapping al indexar.")
                } else {
                    r.with(Info, TypeChanged, format!("JSON como {ty}: se indexa todo como keyword, sin agrandar el mapping."))
                }
            }
            L::Xml => Rendered::exact("text").with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } => Rendered::exact("keyword").with(Info, TypeApproximated, format!("Enumerado como keyword. Valores: {}.", values.join(", "))),
            L::Set { .. } => Rendered::exact("keyword").with(Info, TypeChanged, "Conjunto como keyword: cada campo admite varios valores."),
            L::Array { of } => {
                let inner = self.render_type(of);
                inner.with(Info, TypeChanged, "Arreglo: cada campo admite varios valores, así que queda el tipo de los elementos.")
            }
            L::Geometry { kind: Some(k), .. } if k == "point" => Rendered::exact("geo_point").with(Warning, TypeApproximated, "Punto como geo_point: los valores van como lat/lon, WKT o GeoJSON."),
            L::Geometry { .. } => Rendered::exact("geo_shape").with(Warning, TypeApproximated, "Dato espacial como geo_shape: los valores van como WKT o GeoJSON."),
            L::Inet => Rendered::exact("ip"),
            L::MacAddr => Rendered::exact("keyword"),
            L::RowVersion => Rendered::exact("binary").with(Warning, TypeApproximated, "No hay versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
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
            // Every field is indexed by itself; the designer has no indexes.
            indexes: false,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: false,
            nullability: false,
            comments: true,
            max_identifier: 255,
            case: IdentCase::Lower,
        }
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        let source_name = t.name.clone();
        if t.kind != dbine_driver::kinds::INDEX {
            t.kind = dbine_driver::kinds::INDEX.into();
            let name = index_name(&t.name);
            if name != t.name {
                report.push(Severity::Info, IssueCode::IdentifierRenamed, &source_name, Some(&source_name), format!("Los índices van en minúsculas y sin caracteres especiales: «{}» pasa a «{name}».", t.name));
                t.name = name;
            }
            if let Some(pk) = t.primary_key.take().filter(|k| !k.columns.is_empty()) {
                report.push(
                    Severity::Info,
                    IssueCode::PrimaryKeyDropped,
                    &source_name,
                    None,
                    format!("Los documentos se identifican por _id: al copiar conviene usar la clave ({}) como _id.", pk.columns.join(", ")),
                );
            }
        }
        for c in &mut t.columns {
            let ty = c.data_type.trim().to_string();
            if let Some(f) = ty.strip_prefix("scaled_float(").and_then(|r| r.strip_suffix(')')) {
                c.options.insert("scaling_factor".into(), f.to_string());
                c.data_type = "scaled_float".into();
            } else if (ty == "date" || ty == "date_nanos") && !c.options.contains_key("format") {
                c.options.insert("format".into(), DATE_FORMAT.into());
            }
        }
    }
}

/// An index name Elasticsearch accepts: lower case, without `\ / * ? " < >
/// | , # :` or spaces, not starting with `-`, `_` or `+`.
fn index_name(name: &str) -> String {
    let mut s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if "\\/*?\"<>|,#: ".contains(c) { '_' } else { c })
        .collect();
    while s.starts_with(['-', '_', '+']) {
        s.remove(0);
    }
    if s.is_empty() {
        s = "indice".into();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn parses_mapping_types() {
        let d = lookup("elasticsearch").unwrap();
        let p = |s: &str| d.parse_type(&parse(s));
        assert_eq!(p("keyword"), L::Text { unicode: true });
        assert_eq!(p("text"), L::Text { unicode: true });
        assert_eq!(p("long"), L::int(8));
        assert_eq!(p("integer"), L::int(4));
        assert_eq!(p("short"), L::int(2));
        assert_eq!(p("byte"), L::int(1));
        assert_eq!(p("unsigned_long"), L::Int { bytes: 8, unsigned: true });
        assert_eq!(p("scaled_float"), L::Decimal { precision: None, scale: None });
        assert_eq!(p("half_float"), L::Float { bytes: 4 });
        assert_eq!(p("date"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(p("date_nanos"), L::Timestamp { precision: Some(9), tz: true });
        assert_eq!(p("ip"), L::Inet);
        assert_eq!(p("object"), L::Json { binary: true });
        assert_eq!(p("nested"), L::Json { binary: true });
        assert!(matches!(p("geo_point"), L::Geometry { .. }));
        assert!(matches!(p("alias"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let d = lookup("elasticsearch").unwrap();
        let r = |t: L| d.render_type(&t).native;
        assert_eq!(r(L::Bool), "boolean");
        assert_eq!(r(L::int(2)), "short");
        assert_eq!(r(L::int(16)), "double");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "long");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "unsigned_long");
        assert_eq!(lookup("opendistro").unwrap().render_type(&L::Int { bytes: 8, unsigned: true }).native, "double");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "scaled_float(100)");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(0) }), "long");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "double");
        assert_eq!(r(L::Float { bytes: 4 }), "float");
        assert_eq!(r(L::Money), "scaled_float(10000)");
        assert_eq!(r(L::Char { len: Some(2), unicode: true }), "keyword");
        assert_eq!(r(L::Varchar { len: Some(256), unicode: true }), "keyword");
        assert_eq!(r(L::Varchar { len: Some(257), unicode: true }), "text");
        assert_eq!(r(L::Varchar { len: None, unicode: true }), "text");
        assert_eq!(r(L::Text { unicode: true }), "text");
        assert_eq!(r(L::Blob), "binary");
        assert_eq!(r(L::Bit { len: None }), "keyword");
        assert_eq!(r(L::Date), "date");
        assert_eq!(r(L::Time { precision: None, tz: false }), "keyword");
        assert_eq!(r(L::Timestamp { precision: Some(3), tz: true }), "date");
        assert_eq!(r(L::Timestamp { precision: Some(6), tz: true }), "date_nanos");
        assert_eq!(r(L::Interval), "keyword");
        assert_eq!(r(L::Year), "short");
        assert_eq!(r(L::Uuid), "keyword");
        assert_eq!(r(L::Json { binary: true }), "flattened");
        assert_eq!(lookup("opensearch").unwrap().render_type(&L::Json { binary: true }).native, "flat_object");
        assert_eq!(r(L::Xml), "text");
        assert_eq!(r(L::Enum { values: vec![] }), "keyword");
        assert_eq!(r(L::Set { values: vec![] }), "keyword");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }), "integer");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "flattened");
        assert_eq!(r(L::Geometry { kind: Some("point".into()), srid: None, geography: false }), "geo_point");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "geo_shape");
        assert_eq!(r(L::Inet), "ip");
        assert_eq!(r(L::MacAddr), "keyword");
        assert_eq!(r(L::RowVersion), "binary");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
    }

    #[test]
    fn index_names() {
        assert_eq!(index_name("Ventas"), "ventas");
        assert_eq!(index_name("_mis ventas"), "mis_ventas");
    }
}
