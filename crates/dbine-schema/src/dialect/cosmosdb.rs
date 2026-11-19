//! Azure Cosmos DB (NoSQL API). Containers hold schemaless JSON (the type
//! model is in [`super::couchdb`]); every number is an IEEE double.
//!
//! A container needs a partition key, which the conversion takes from the
//! primary key: its columns (up to three, a hierarchical key) as paths.
//! Each row is then its own logical partition, which spreads writes and
//! keeps point reads by key cheap. Cosmos DB identifies a document by its
//! `id` (text, unique within its partition), so a key that isn't `id` is
//! reported: the copy has to fill `id`.
//!
//! Indexes: Cosmos DB indexes every path by itself, so single-field
//! indexes are dropped; non-unique multi-field ones are composite indexes;
//! unique ones become the unique key policy, which is checked only within
//! each logical partition.

use super::couchdb::{
    json_caps, map_vector_index, parse_json_type, parse_vector_field, render_json_type,
    render_vector_field, schemaless_note,
};
use super::{Caps, Dialect, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct CosmosDb;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: CosmosDb = CosmosDb;
    (driver_id == "cosmosdb").then_some(&D as &dyn Dialect)
}

const PARTITION_KEY: &str = "partition_key";

/// Most dimensions a vector index takes (`quantizedFlat`, `diskANN`; the
/// driver writes a `flat` one over 505 as `quantizedFlat`).
const MAX_DIMS: u32 = 4096;

/// The vector element types of Cosmos DB's embedding policy, besides
/// float32.
const VECTOR_ELEMENTS: &[(&str, dbine_driver::VectorElement)] = &[
    ("int8", dbine_driver::VectorElement::I8),
    ("uint8", dbine_driver::VectorElement::U8),
];

impl Dialect for CosmosDb {
    fn id(&self) -> &'static str {
        "cosmosdb"
    }

    /// A field of the container's vector embedding policy is `vector(N)`
    /// (float32), `vector(N, int8)` or `vector(N, uint8)`.
    fn parse_type(&self, t: &TypeSpec) -> L {
        parse_vector_field(&t.raw, VECTOR_ELEMENTS).unwrap_or_else(|| parse_json_type(t))
    }

    fn render_type(&self, t: &L) -> Rendered {
        if let L::Vector { element, .. } = t {
            use dbine_driver::VectorElement as E;
            if let Some(r) = render_vector_field(t, VECTOR_ELEMENTS) {
                return match element {
                    E::F32 | E::I8 | E::U8 => r,
                    E::Bit => r.with(
                        Severity::Warning,
                        IssueCode::TypeApproximated,
                        "Cosmos DB no tiene vectores de bits: quedan como float32 (0 y 1).",
                    ),
                    _ => r.with(
                        Severity::Info,
                        IssueCode::TypeChanged,
                        "Cosmos DB guarda los vectores como float32, int8 o uint8: queda float32.",
                    ),
                };
            }
        }
        render_json_type(t, true)
    }

    /// `flat`, `quantizedFlat` or `diskANN`, with the embedding's distance
    /// (`cosine`, `euclidean` = `l2`, `dotproduct` = `ip`). The embedding
    /// policy states the vector's dimension: a column without one can't
    /// have it, nor one wider than [`MAX_DIMS`].
    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
        column: Option<&crate::logical::LogicalType>,
    ) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
        use dbine_driver::vector::metric;
        match column {
            Some(L::Vector { dims: None, .. }) => {
                return Err(
                    "su columna vectorial no tiene dimensión, y Cosmos DB la necesita".into(),
                )
            }
            Some(L::Vector { dims: Some(n), .. }) if *n > MAX_DIMS => return Err(format!(
                "Cosmos DB indexa vectores de hasta {MAX_DIMS} dimensiones, y su columna tiene {n}"
            )),
            _ => {}
        }
        let bits = matches!(
            column,
            Some(L::Vector {
                element: dbine_driver::VectorElement::Bit,
                ..
            })
        );
        // The squared euclidean distance (Couchbase's `l2_squared`) has the
        // same nearest neighbors as euclidean, and so has hamming over bits
        // (stored as 0 and 1): they go on, and the driver writes euclidean
        // and says so.
        let squared = v.metric.as_deref().is_some_and(|m| {
            let m = m.to_ascii_lowercase();
            matches!(m.as_str(), "l2_squared" | "euclidean_squared")
                || (bits && m == metric::HAMMING)
        });
        let mut asked = v.clone();
        if squared {
            asked.metric = Some(metric::L2.into());
        }
        let (kind, mut out) = map_vector_index(
            &asked,
            "Cosmos DB",
            &[metric::COSINE, metric::L2, metric::IP],
            &["flat", "quantizedFlat", "diskANN"],
            &[],
        )?;
        if squared {
            out.metric = v.metric.as_deref().map(str::to_ascii_lowercase);
        }
        Ok((kind, out))
    }

    fn render_default(&self, _d: &DefaultValue, _ty: &L) -> Option<String> {
        None
    }

    fn caps(&self) -> Caps {
        json_caps(255)
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        if t.options.contains_key(PARTITION_KEY) {
            return;
        }
        let name = t.name.clone();
        t.kind = dbine_driver::kinds::COLLECTION.into();
        schemaless_note(t, report, "Cosmos DB");
        let key: Vec<String> = t
            .primary_key
            .as_ref()
            .map(|k| k.columns.clone())
            .unwrap_or_default();
        let paths: Vec<String> = if key.is_empty() {
            vec!["id".into()]
        } else {
            key.iter().take(3).cloned().collect()
        };
        let pk = paths
            .iter()
            .map(|p| format!("/{p}"))
            .collect::<Vec<_>>()
            .join(", ");
        t.options.insert(PARTITION_KEY.into(), pk.clone());
        report.push(
            Severity::Info,
            IssueCode::OptionAdded,
            &name,
            Some(PARTITION_KEY),
            format!("Clave de partición {pk}, tomada de la clave primaria: cada fila es su propia partición lógica."),
        );
        if key.len() > 3 {
            report.push(Severity::Warning, IssueCode::OptionAdded, &name, Some(PARTITION_KEY), "La clave de partición jerárquica admite hasta 3 rutas: se usan las 3 primeras columnas de la clave.");
        }
        match key.as_slice() {
            [] => report.push(
                Severity::Warning,
                IssueCode::PrimaryKeyAdded,
                &name,
                None,
                "La tabla no tiene clave primaria: Cosmos DB identifica los documentos por «id», que hay que generar al copiar.",
            ),
            [k] if k == "id" => {}
            _ => report.push(
                Severity::Warning,
                IssueCode::PrimaryKeyDropped,
                &name,
                None,
                format!("Cosmos DB identifica cada documento por «id» (texto): al copiar hay que llenarlo con la clave ({}).", key.join(", ")),
            ),
        }
        let mut kept = Vec::new();
        for mut ix in std::mem::take(&mut t.indexes) {
            if ix.unique {
                report.push(
                    Severity::Warning,
                    IssueCode::IndexChanged,
                    &name,
                    Some(&ix.name),
                    "Clave única de Cosmos DB: solo se controla dentro de cada partición lógica, y no se puede cambiar después de crear el contenedor.",
                );
                kept.push(ix);
            } else if ix.columns.len() < 2 {
                report.push(
                    Severity::Info,
                    IssueCode::IndexDropped,
                    &name,
                    Some(&ix.name),
                    "Cosmos DB ya indexa cada campo: el índice simple no hace falta.",
                );
            } else {
                ix.kind = Some("composite".into());
                kept.push(ix);
            }
        }
        t.indexes = kept;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn types() {
        let d = CosmosDb;
        assert_eq!(
            d.parse_type(&parse("integer|number")),
            L::Float { bytes: 8 }
        );
        assert_eq!(d.render_type(&L::int(8)).native, "integer");
        assert!(!d.render_type(&L::int(8)).notes.is_empty());
        assert!(!d.caps().nullability && !d.caps().foreign_keys);
    }

    #[test]
    fn vector_fields_and_indexes() {
        use dbine_driver::{VectorElement as E, VectorIndex};
        let d = CosmosDb;
        let v = |dims, element| L::Vector {
            dims,
            element,
            sparse: false,
        };
        assert_eq!(d.parse_type(&parse("vector(1536)")), v(Some(1536), E::F32));
        assert_eq!(d.parse_type(&parse("vector(3, uint8)")), v(Some(3), E::U8));
        assert_eq!(
            d.parse_type(&parse("array|vector(3, int8)")),
            v(Some(3), E::I8)
        );
        assert_eq!(d.parse_type(&parse("array")), L::Json { binary: true });
        // pgvector's `vector(3)` (the logical vector) and back.
        for (t, native, notes) in [
            (v(Some(3), E::F32), "vector(3)", false),
            (v(Some(3), E::U8), "vector(3, uint8)", false),
            (v(Some(3), E::I8), "vector(3, int8)", false),
            (v(Some(3), E::F16), "vector(3)", true),
            (v(Some(8), E::Bit), "vector(8)", true),
        ] {
            let r = d.render_type(&t);
            assert_eq!(r.native, native);
            assert_eq!(!r.notes.is_empty(), notes, "{native}");
        }
        assert_eq!(d.parse_type(&parse("vector(3)")), v(Some(3), E::F32));
        // A sparse vector has no embedding: an array with a warning.
        let sparse = d.render_type(&L::Vector {
            dims: Some(5),
            element: E::F32,
            sparse: true,
        });
        assert_eq!(sparse.native, "array");
        assert!(!sparse.notes.is_empty());
        // Indexes: the distance stays, the algorithm only when Cosmos has it.
        let hnsw = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("ip".into()),
            params: [("m".to_string(), "16".to_string())].into(),
        };
        let (kind, out) = d.vector_index(&hnsw, None).unwrap();
        assert_eq!(
            (kind, out.algorithm, out.metric.as_deref()),
            (None, None, Some("ip"))
        );
        assert!(out.params.is_empty());
        let disk = VectorIndex {
            algorithm: Some("diskann".into()),
            ..Default::default()
        };
        assert_eq!(
            d.vector_index(&disk, None).unwrap().1.algorithm.as_deref(),
            Some("diskANN")
        );
        let hamming = VectorIndex {
            metric: Some("hamming".into()),
            ..Default::default()
        };
        assert!(d.vector_index(&hamming, None).is_err());
        // Over bits (0 and 1 here) hamming goes on, for the driver to
        // write as euclidean, which ranks them the same.
        let bit_col = |dims| L::Vector {
            dims,
            element: dbine_driver::VectorElement::Bit,
            sparse: false,
        };
        assert_eq!(
            d.vector_index(&hamming, Some(&bit_col(Some(8))))
                .unwrap()
                .1
                .metric
                .as_deref(),
            Some("hamming")
        );
        let f32_col = L::Vector {
            dims: Some(8),
            element: dbine_driver::VectorElement::F32,
            sparse: false,
        };
        assert!(d.vector_index(&hamming, Some(&f32_col)).is_err());
        // Couchbase's squared euclidean goes on (the driver writes
        // euclidean and says so).
        let sq = VectorIndex {
            algorithm: Some("diskANN".into()),
            metric: Some("l2_squared".into()),
            ..Default::default()
        };
        let (_, out) = d.vector_index(&sq, None).unwrap();
        assert_eq!(
            (out.algorithm.as_deref(), out.metric.as_deref()),
            (Some("diskANN"), Some("l2_squared"))
        );
        // The column it searches: the embedding policy needs its dimension.
        let col = |dims| L::Vector {
            dims,
            element: dbine_driver::VectorElement::F32,
            sparse: false,
        };
        assert!(d.vector_index(&disk, Some(&col(Some(1536)))).is_ok());
        let why = d.vector_index(&disk, Some(&col(None))).unwrap_err();
        assert!(why.contains("no tiene dimensión"), "{why}");
        // Up to 4096 dimensions (an Oracle VECTOR(8192) is dropped).
        assert!(d.vector_index(&disk, Some(&col(Some(4096)))).is_ok());
        let why = d.vector_index(&disk, Some(&col(Some(8192)))).unwrap_err();
        assert!(why.contains("hasta 4096 dimensiones"), "{why}");
    }
}
