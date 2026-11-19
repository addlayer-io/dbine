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

use super::couchdb::{
    json_caps, key_as_index, map_vector_index, parse_json_type, parse_vector_field,
    render_json_type, render_vector_field, schemaless_note, unique_to_plain,
};
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

    /// A vector index's field is `vector(N)` (float32 arrays).
    fn parse_type(&self, t: &TypeSpec) -> L {
        parse_vector_field(&t.raw, &[]).unwrap_or_else(|| parse_json_type(t))
    }

    fn render_type(&self, t: &L) -> Rendered {
        if let L::Vector { element, .. } = t {
            use dbine_driver::VectorElement as E;
            if let Some(r) = render_vector_field(t, &[]) {
                return match element {
                    E::F32 => r,
                    _ => r.with(Severity::Info, IssueCode::TypeChanged, "Los índices vectoriales de Couchbase toman arreglos de float32: los números se guardan como tales."),
                };
            }
        }
        render_json_type(t, false)
    }

    /// GSI vector indexes (hyperscale by default): `cosine`, `l2`,
    /// `l2_squared`, `ip` (DOT); the number of IVF lists carries over. The
    /// index states its vector's `dimension`: a column without one can't
    /// have it.
    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
        column: Option<&crate::logical::LogicalType>,
    ) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
        use dbine_driver::vector::{metric, param};
        if let Some(L::Vector { dims: None, .. }) = column {
            return Err("su columna vectorial no tiene dimensión, y Couchbase la necesita".into());
        }
        map_vector_index(
            v,
            "Couchbase",
            &[metric::COSINE, metric::L2, "l2_squared", metric::IP],
            &[],
            &[param::LISTS],
        )
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
        assert_eq!(
            d.parse_type(&parse("integer | string")),
            L::Json { binary: true }
        );
        assert_eq!(d.parse_type(&parse("string")), L::Text { unicode: true });
        assert_eq!(d.render_type(&L::int(8)).native, "integer");
        assert!(d.render_type(&L::int(8)).notes.is_empty());
    }

    #[test]
    fn vector_fields_and_indexes() {
        use dbine_driver::{VectorElement as E, VectorIndex};
        let d = Couchbase;
        let v = |dims, element| L::Vector {
            dims,
            element,
            sparse: false,
        };
        assert_eq!(d.parse_type(&parse("vector(1536)")), v(Some(1536), E::F32));
        assert_eq!(
            d.parse_type(&parse("array | vector(3)")),
            v(Some(3), E::F32)
        );
        assert_eq!(d.parse_type(&parse("array")), L::Json { binary: true });
        // pgvector's `vector(3)` (the logical vector) and back.
        let r = d.render_type(&v(Some(3), E::F32));
        assert_eq!((r.native.as_str(), r.notes.len()), ("vector(3)", 0));
        assert_eq!(d.parse_type(&parse(&r.native)), v(Some(3), E::F32));
        let half = d.render_type(&v(Some(3), E::F16));
        assert_eq!(half.native, "vector(3)");
        assert!(!half.notes.is_empty());
        let ivf = VectorIndex {
            algorithm: Some("ivfflat".into()),
            metric: Some("l2".into()),
            params: [("lists", "100"), ("probes", "10")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let (kind, out) = d.vector_index(&ivf, None).unwrap();
        assert_eq!((kind, out.algorithm), (None, None));
        assert_eq!(out.metric.as_deref(), Some("l2"));
        assert_eq!(out.params.len(), 1);
        let jaccard = VectorIndex {
            metric: Some("jaccard".into()),
            ..Default::default()
        };
        assert!(d.vector_index(&jaccard, None).is_err());
        // The column it searches: the index needs its dimension.
        let col = |dims| L::Vector {
            dims,
            element: E::F32,
            sparse: false,
        };
        assert!(d.vector_index(&ivf, Some(&col(Some(3)))).is_ok());
        let why = d.vector_index(&ivf, Some(&col(None))).unwrap_err();
        assert!(why.contains("no tiene dimensión"), "{why}");
    }
}
