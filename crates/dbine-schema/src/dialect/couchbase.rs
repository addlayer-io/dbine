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

/// Most dimensions a vector index takes: GSI's `dimension` and a Search
/// vector field's `dims` (7.6.2 and later).
const MAX_DIMS: u32 = 4096;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Couchbase = Couchbase;
    (driver_id == "couchbase").then_some(&D as &dyn Dialect)
}

impl Dialect for Couchbase {
    fn id(&self) -> &'static str {
        "couchbase"
    }

    /// A vector index's field is `vector(N)` (float32 arrays), or
    /// `vector(N, base64)` (float32s as base64 text: the driver reads and
    /// writes them as numbers, so it moves as any float32 vector; another
    /// engine gets an array).
    fn parse_type(&self, t: &TypeSpec) -> L {
        parse_vector_field(&t.raw, &[("base64", dbine_driver::VectorElement::F32)])
            .unwrap_or_else(|| parse_json_type(t))
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
    /// have it, nor one wider than [`MAX_DIMS`]. Over bits (stored here as
    /// 0 and 1) `hamming` goes on, for the driver to write as
    /// `L2_SQUARED`, which on 0/1 vectors is the same distance.
    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
        ctx: &super::VectorIndexCtx<'_>,
    ) -> Result<super::VectorIndexMapping, String> {
        let column = ctx.column;
        use dbine_driver::vector::{metric, param};
        match column {
            Some(L::Vector { dims: None, .. }) => {
                return Err(
                    "su columna vectorial no tiene dimensión, y Couchbase la necesita".into(),
                )
            }
            Some(L::Vector { dims: Some(n), .. }) if *n > MAX_DIMS => {
                return Err(format!(
                "Couchbase indexa vectores de hasta {MAX_DIMS} dimensiones, y su columna tiene {n}"
            ))
            }
            _ => {}
        }
        let bits = matches!(
            column,
            Some(L::Vector {
                element: dbine_driver::VectorElement::Bit,
                ..
            })
        );
        let mut metrics = vec![metric::COSINE, metric::L2, "l2_squared", metric::IP];
        if bits {
            metrics.push(metric::HAMMING);
        }
        map_vector_index(v, "Couchbase", &metrics, &[], &[param::LISTS]).map(Into::into)
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
    use crate::dialect::VectorIndexPair as _;
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
        // Base64 text of float32s: a float32 vector (an array elsewhere).
        assert_eq!(
            d.parse_type(&parse("vector(3, base64)")),
            v(Some(3), E::F32)
        );
        assert_eq!(d.parse_type(&parse("vector(base64)")), v(None, E::F32));
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
        let (kind, out) = d.vector_index_pair(&ivf, None).unwrap();
        assert_eq!((kind, out.algorithm), (None, None));
        assert_eq!(out.metric.as_deref(), Some("l2"));
        assert_eq!(out.params.len(), 1);
        let jaccard = VectorIndex {
            metric: Some("jaccard".into()),
            ..Default::default()
        };
        assert!(d.vector_index_pair(&jaccard, None).is_err());
        // The column it searches: the index needs its dimension.
        let col = |dims| L::Vector {
            dims,
            element: E::F32,
            sparse: false,
        };
        assert!(d.vector_index_pair(&ivf, Some(&col(Some(3)))).is_ok());
        let why = d.vector_index_pair(&ivf, Some(&col(None))).unwrap_err();
        assert!(why.contains("no tiene dimensión"), "{why}");
        // Up to 4096 dimensions (an Oracle VECTOR(8192) is dropped).
        assert!(d.vector_index_pair(&ivf, Some(&col(Some(4096)))).is_ok());
        let why = d
            .vector_index_pair(&ivf, Some(&col(Some(8192))))
            .unwrap_err();
        assert!(why.contains("hasta 4096 dimensiones"), "{why}");
        // Hamming over bits goes on (the driver writes L2_SQUARED), over
        // anything else it doesn't.
        let hamming = VectorIndex {
            metric: Some("hamming".into()),
            ..Default::default()
        };
        let bits = L::Vector {
            dims: Some(8),
            element: E::Bit,
            sparse: false,
        };
        assert_eq!(
            d.vector_index_pair(&hamming, Some(&bits))
                .unwrap()
                .1
                .metric
                .as_deref(),
            Some("hamming")
        );
        assert!(d.vector_index_pair(&hamming, Some(&col(Some(8)))).is_err());
        assert!(d.vector_index_pair(&hamming, None).is_err());
    }

    #[test]
    fn base64_fields_move_as_vectors() {
        use dbine_driver::{ColumnDef, IndexDef, VectorIndex};
        // A Search `vector_base64` field as the driver describes it.
        let t = TableSchema {
            kind: dbine_driver::kinds::COLLECTION.into(),
            schema: Some("b.s".into()),
            name: "docs".into(),
            columns: vec![
                ColumnDef {
                    name: "_id".into(),
                    data_type: "string".into(),
                    ..Default::default()
                },
                ColumnDef {
                    name: "e".into(),
                    data_type: "vector(3, base64)".into(),
                    nullable: true,
                    ..Default::default()
                },
            ],
            primary_key: Some(dbine_driver::KeyDef {
                columns: vec!["_id".into()],
                ..Default::default()
            }),
            indexes: vec![IndexDef {
                name: "evs".into(),
                columns: vec!["e".into()],
                kind: Some("SEARCH".into()),
                vector: Some(VectorIndex {
                    metric: Some("l2".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = crate::convert::Options::default();
        let c = crate::convert::convert(std::slice::from_ref(&t), "couchbase", "postgres", &opts)
            .unwrap();
        let pg = &c.tables[0];
        assert_eq!(pg.columns[1].data_type, "vector(3)");
        assert_eq!(pg.indexes.len(), 1, "{:?}", c.issues);
        assert_eq!(
            pg.indexes[0].vector.as_ref().unwrap().metric.as_deref(),
            Some("l2")
        );
        // Couchbase to Couchbase keeps the form.
        let c = crate::convert::convert(&[t], "couchbase", "couchbase", &opts).unwrap();
        assert_eq!(c.tables[0].columns[1].data_type, "vector(3, base64)");
    }
}
