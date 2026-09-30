//! Couchbase. Collections hold schemaless JSON (the type model is in
//! [`super::couchdb`]); the driver reports `_id` (the document key,
//! `META().id`) plus the fields of a sample, types joined with ` | `.
//!
//! From SQL: the collection lives in a `bucket.scope` the source doesn't
//! know, so a schema without a dot is cleared and reported (the target
//! schema has to be chosen). The document key is what the copy writes in
//! `id` / `_id` (or a UUID), so the primary key becomes a plain index on
//! its fields; GSI indexes have no uniqueness. A primary index is added so
//! the collection can be browsed and queried without other indexes.

use super::couchdb::{json_caps, key_as_index, parse_json_type, render_json_type, schemaless_note, unique_to_plain};
use super::{Caps, Dialect, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct Couchbase;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Couchbase = Couchbase;
    (driver_id == "couchbase").then_some(&D as &dyn Dialect)
}

impl Dialect for Couchbase {
    fn id(&self) -> &'static str {
        "couchbase"
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
        // Collection names: up to 251 characters.
        json_caps(251)
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        // Read from Couchbase: `_id` is the document key.
        if t.primary_key.as_ref().is_some_and(|k| k.columns == ["_id"]) {
            return;
        }
        let name = t.name.clone();
        t.kind = dbine_driver::kinds::COLLECTION.into();
        // Across engines the converter already cleared the source schema.
        if t.schema.as_deref().is_none_or(|s| !s.contains('.')) {
            let s = t.schema.take().unwrap_or_default();
            report.push(
                Severity::Warning,
                IssueCode::OptionDropped,
                &name,
                (!s.is_empty()).then_some(s.as_str()),
                "Hay que elegir el bucket.scope de Couchbase donde crear la colección.",
            );
        }
        schemaless_note(t, report, "Couchbase");
        key_as_index(t, report, "Cada documento se identifica por su clave (META().id), que la copia toma de «id» o «_id».");
        unique_to_plain(t, report, "Couchbase");
        t.options.insert("primary_index".into(), "true".into());
        report.push(
            Severity::Info,
            IssueCode::OptionAdded,
            &name,
            Some("primary_index"),
            "Se crea el índice primario, para poder consultar la colección sin otros índices.",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn types() {
        let d = Couchbase;
        assert_eq!(d.parse_type(&parse("integer | string")), L::Json { binary: true });
        assert_eq!(d.parse_type(&parse("string")), L::Text { unicode: true });
        assert_eq!(d.render_type(&L::int(8)).native, "integer");
        assert!(d.render_type(&L::int(8)).notes.is_empty());
    }
}
