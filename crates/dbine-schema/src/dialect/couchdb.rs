//! CouchDB, plus the JSON-document type model it shares with Cosmos DB and
//! Couchbase.
//!
//! These engines store schemaless JSON: their drivers report the fields of
//! a sample with the JSON types seen (`string`, `integer`, `number`,
//! `boolean`, `object`, `array`, several joined with `|`), and nothing in
//! the target checks a type. So the types rendered for them only document
//! what the data will look like, and a note per table says so.
//!
//! CouchDB has no tables: the container of documents is the database
//! itself, and `database_schema` reports one `_all_docs` pseudo-table. A
//! SQL table converted to CouchDB only brings its indexes (Mango indexes,
//! created in the session's database); its documents would share the
//! database with those of every other table. It works as a source; as a
//! target the report says what is left out.

use super::mongodb::parse_list;
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{IndexDef, TableSchema};

pub struct CouchDb;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: CouchDb = CouchDb;
    (driver_id == "couchdb").then_some(&D as &dyn Dialect)
}

fn json_type(t: &str) -> L {
    match t {
        "boolean" | "bool" => L::Bool,
        "integer" | "int" => L::int(8),
        "number" | "float" | "double" => L::Float { bytes: 8 },
        "string" => L::Text { unicode: true },
        "object" | "array" | "any" | "json" => L::Json { binary: true },
        _ => L::Other { native: t.to_string() },
    }
}

/// A JSON-document type (`string`, `integer | string`…) as a logical one.
pub(crate) fn parse_json_type(t: &TypeSpec) -> L {
    let raw = t.raw.trim();
    if raw.is_empty() {
        return L::Json { binary: true };
    }
    match parse_list(raw, json_type) {
        L::Other { .. } => L::Other { native: raw.to_string() },
        l => l,
    }
}

/// A logical type as the JSON type its values take in a document.
/// `doubles_only`: the engine keeps every number as an IEEE double
/// (Cosmos DB), so integers beyond 2^53 lose precision.
pub(crate) fn render_json_type(t: &L, doubles_only: bool) -> Rendered {
    use IssueCode::*;
    use Severity::*;
    match t {
        L::Bool => Rendered::exact("boolean"),
        L::Int { bytes, unsigned } => {
            let r = Rendered::exact("integer");
            if L::signed_bytes_for(*bytes, *unsigned) > 8 || (doubles_only && L::signed_bytes_for(*bytes, *unsigned) > 4) {
                r.with(Loss, RangeLoss, "Los números JSON del destino son de coma flotante: los enteros de más de 15 dígitos pierden precisión.")
            } else {
                r
            }
        }
        L::Decimal { precision: Some(p), .. } if *p <= 15 => {
            Rendered::exact("number").with(Info, TypeChanged, "Número JSON: se guarda en coma flotante, exacto hasta 15 dígitos.")
        }
        L::Decimal { .. } => Rendered::exact("number").with(Loss, PrecisionLoss, "Número JSON de coma flotante: más de 15 dígitos significativos pierden precisión."),
        L::Float { .. } => Rendered::exact("number"),
        L::Money => Rendered::exact("number").with(Info, TypeChanged, "Moneda como número JSON (coma flotante)."),
        L::Char { .. } | L::Varchar { .. } | L::Text { .. } => Rendered::exact("string"),
        L::Binary { .. } | L::Varbinary { .. } | L::Blob => {
            Rendered::exact("string").with(Warning, TypeApproximated, "JSON no tiene binarios: el valor va como texto (hexadecimal o base64).")
        }
        L::Bit { .. } => Rendered::exact("string").with(Warning, TypeApproximated, "Cadena de bits como texto de 0 y 1."),
        L::Date | L::Time { .. } | L::Timestamp { .. } | L::Interval => {
            Rendered::exact("string").with(Info, TypeChanged, "JSON no tiene fechas: el valor va como texto ISO 8601.")
        }
        L::Year => Rendered::exact("integer"),
        L::Uuid => Rendered::exact("string"),
        L::Json { .. } | L::Map { .. } => Rendered::exact("object"),
        L::Xml => Rendered::exact("string"),
        L::Enum { values } => Rendered::exact("string").with(Info, TypeApproximated, format!("Enumerado como texto. Valores: {}.", values.join(", "))),
        L::Set { .. } | L::Array { .. } => Rendered::exact("array"),
        L::Geometry { .. } => Rendered::exact("object").with(Warning, TypeApproximated, "Dato espacial como objeto: hay que convertir los valores a GeoJSON."),
        L::Inet | L::MacAddr => Rendered::exact("string"),
        L::RowVersion => Rendered::exact("string").with(Warning, TypeApproximated, "No hay versión de fila automática: el valor se copia como texto y no se actualiza solo."),
        L::Other { native } => Rendered::exact(native.clone()),
    }
}

/// What the JSON-document engines hold: indexes, and nothing that checks
/// a document.
pub(crate) fn json_caps(max_identifier: usize) -> Caps {
    Caps {
        foreign_keys: false,
        on_delete: &[],
        on_update: &[],
        indexes: true,
        partial_indexes: false,
        supports_include: false,
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: false,
        max_identifier,
        case: IdentCase::Lower,
    }
}

/// The note every converted table gets: its types aren't stored.
pub(crate) fn schemaless_note(t: &TableSchema, report: &mut Report, engine: &str) {
    report.push(
        Severity::Info,
        IssueCode::TypeChanged,
        &t.name,
        None,
        format!("{engine} no guarda un esquema: los tipos indican cómo quedan los valores en los documentos, nada los controla."),
    );
}

/// Unique indexes become plain ones on engines without unique indexes.
pub(crate) fn unique_to_plain(t: &mut TableSchema, report: &mut Report, engine: &str) {
    let name = t.name.clone();
    for ix in t.indexes.iter_mut().filter(|i| i.unique) {
        ix.unique = false;
        report.push(
            Severity::Warning,
            IssueCode::IndexChanged,
            &name,
            Some(&ix.name),
            format!("{engine} no tiene índices únicos: el índice queda común y la unicidad no se controla."),
        );
    }
}

/// The primary key as a plain index named `<table>_pk` (unless an index
/// on those fields already exists), for engines whose documents are
/// identified by their own key.
pub(crate) fn key_as_index(t: &mut TableSchema, report: &mut Report, why: &str) {
    let Some(pk) = t.primary_key.take().filter(|k| !k.columns.is_empty()) else { return };
    let name = format!("{}_pk", t.name);
    report.push(Severity::Warning, IssueCode::PrimaryKeyDropped, &t.name, Some(&name), format!("{why} La clave primaria ({}) queda como índice «{name}», sin controlar la unicidad.", pk.columns.join(", ")));
    if !t.indexes.iter().any(|i| i.columns == pk.columns) {
        t.indexes.insert(0, IndexDef { name, columns: pk.columns, ..Default::default() });
    }
}

impl Dialect for CouchDb {
    fn id(&self) -> &'static str {
        "couchdb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        parse_json_type(t)
    }

    fn render_type(&self, t: &L) -> Rendered {
        render_json_type(t, false)
    }

    fn render_default(&self, _d: &DefaultValue, _ty: &L) -> Option<String> {
        None
    }

    fn caps(&self) -> Caps {
        json_caps(238)
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        if t.name == "_all_docs" {
            return;
        }
        t.kind = dbine_driver::kinds::COLLECTION.into();
        report.push(
            Severity::Warning,
            IssueCode::TableChanged,
            &t.name,
            None,
            "CouchDB no tiene tablas: los documentos van a la base de la sesión, junto con los de las demás tablas (conviene un campo que diga de qué tabla vienen). Del esquema solo se crean los índices Mango.",
        );
        schemaless_note(t, report, "CouchDB");
        key_as_index(t, report, "Cada documento se identifica por su _id.");
        unique_to_plain(t, report, "CouchDB");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn parses_json_types() {
        let d = CouchDb;
        let p = |s: &str| d.parse_type(&parse(s));
        assert_eq!(p("string"), L::Text { unicode: true });
        assert_eq!(p("integer"), L::int(8));
        assert_eq!(p("number"), L::Float { bytes: 8 });
        assert_eq!(p("integer|number"), L::Float { bytes: 8 });
        assert_eq!(p("integer | number"), L::Float { bytes: 8 });
        assert_eq!(p("boolean"), L::Bool);
        assert_eq!(p("object"), L::Json { binary: true });
        assert_eq!(p("array"), L::Json { binary: true });
        assert_eq!(p("string|integer"), L::Json { binary: true });
        assert_eq!(p("null"), L::Text { unicode: true });
        assert!(matches!(p("gizmo"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| render_json_type(&t, false).native;
        assert_eq!(r(L::Bool), "boolean");
        assert_eq!(r(L::int(8)), "integer");
        assert!(render_json_type(&L::int(8), true).notes.iter().any(|n| n.code == IssueCode::RangeLoss));
        assert!(render_json_type(&L::int(4), true).notes.is_empty());
        assert_eq!(r(L::Decimal { precision: Some(10), scale: Some(2) }), "number");
        assert!(render_json_type(&L::Decimal { precision: Some(30), scale: Some(2) }, false).notes[0].severity == Severity::Loss);
        assert_eq!(r(L::Float { bytes: 4 }), "number");
        assert_eq!(r(L::Money), "number");
        assert_eq!(r(L::Char { len: Some(2), unicode: true }), "string");
        assert_eq!(r(L::Blob), "string");
        assert_eq!(r(L::Bit { len: None }), "string");
        assert_eq!(r(L::Date), "string");
        assert_eq!(r(L::Timestamp { precision: None, tz: true }), "string");
        assert_eq!(r(L::Year), "integer");
        assert_eq!(r(L::Uuid), "string");
        assert_eq!(r(L::Json { binary: false }), "object");
        assert_eq!(r(L::Xml), "string");
        assert_eq!(r(L::Enum { values: vec![] }), "string");
        assert_eq!(r(L::Set { values: vec![] }), "array");
        assert_eq!(r(L::Array { of: Box::new(L::Bool) }), "array");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "object");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "object");
        assert_eq!(r(L::Inet), "string");
        assert_eq!(r(L::RowVersion), "string");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
    }
}
