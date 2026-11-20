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

/// Most dimensions a vector index takes (`quantizedFlat`, `diskANN`).
const MAX_DIMS: u32 = 4096;

/// Most dimensions a `flat` vector index takes.
const FLAT_MAX_DIMS: u32 = 505;

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
        ctx: &super::VectorIndexCtx<'_>,
    ) -> Result<super::VectorIndexMapping, String> {
        let column = ctx.column;
        use dbine_driver::vector::metric;
        match column {
            Some(L::Vector { dims: None, .. }) => {
                return Err(
                    "su columna vectorial no tiene dimensión, y Cosmos DB la necesita".into(),
                )
            }
            Some(L::Vector { dims: Some(n), .. }) if *n > MAX_DIMS => {
                return Err(format!(
                "Cosmos DB indexa vectores de hasta {MAX_DIMS} dimensiones, y su columna tiene {n}"
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
        // The squared euclidean distance (Couchbase's `l2_squared`) has the
        // same nearest neighbors as euclidean, and so has hamming over bits
        // (stored as 0 and 1): they become euclidean, said.
        let mut notes = Vec::new();
        let mut asked = v.clone();
        match v.metric.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some(m @ ("l2_squared" | "euclidean_squared")) => {
                notes.push(format!(
                    "la distancia {m} queda euclidean (Cosmos DB no tiene la euclídea al cuadrado; los vecinos más cercanos son los mismos)"
                ));
                asked.metric = Some(metric::L2.into());
            }
            Some(metric::HAMMING) if bits => {
                notes.push(
                    "la distancia hamming queda euclidean (Cosmos DB no tiene hamming; sobre vectores de 0 y 1 los vecinos más cercanos son los mismos)"
                        .to_string(),
                );
                asked.metric = Some(metric::L2.into());
            }
            _ => {}
        }
        let (kind, mut out) = map_vector_index(
            &asked,
            "Cosmos DB",
            &[metric::COSINE, metric::L2, metric::IP],
            &["flat", "quantizedFlat", "diskANN"],
            &[],
        )?;
        // `flat` compares every vector up to 505 dimensions; quantizedFlat
        // does it over their quantized form, past them too.
        if let (Some(true), Some(L::Vector { dims: Some(n), .. })) = (
            out.algorithm
                .as_deref()
                .map(|a| a.eq_ignore_ascii_case("flat")),
            column,
        ) {
            if *n > FLAT_MAX_DIMS {
                out.algorithm = Some("quantizedFlat".into());
                notes.push(format!(
                    "queda quantizedFlat: flat indexa vectores de hasta {FLAT_MAX_DIMS} dimensiones, y este tiene {n}"
                ));
            }
        }
        Ok(super::VectorIndexMapping {
            kind,
            index: out,
            notes,
        })
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
            if ix.vector.is_some() {
                // A vector index: the container's vector policy, one path
                // each (no filter fields).
                if ix.columns.len() > 1 {
                    report.push(
                        Severity::Info,
                        IssueCode::IndexChanged,
                        &name,
                        Some(&ix.name),
                        format!(
                            "Los índices vectoriales de Cosmos DB no tienen campos de filtro: quedan fuera {}.",
                            ix.columns[1..].join(", ")
                        ),
                    );
                    ix.columns.truncate(1);
                }
                kept.push(ix);
            } else if ix.unique {
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
    use crate::dialect::VectorIndexPair as _;
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
        let (kind, out) = d.vector_index_pair(&hnsw, None).unwrap();
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
            d.vector_index_pair(&disk, None)
                .unwrap()
                .1
                .algorithm
                .as_deref(),
            Some("diskANN")
        );
        let hamming = VectorIndex {
            metric: Some("hamming".into()),
            ..Default::default()
        };
        assert!(d.vector_index_pair(&hamming, None).is_err());
        // Over bits (0 and 1 here) hamming becomes euclidean, which ranks
        // them the same, said.
        let bit_col = |dims| L::Vector {
            dims,
            element: dbine_driver::VectorElement::Bit,
            sparse: false,
        };
        let ctx = |column| crate::dialect::VectorIndexCtx {
            column,
            target_driver: Some("cosmosdb"),
        };
        let bits = bit_col(Some(8));
        let m = d.vector_index(&hamming, &ctx(Some(&bits))).unwrap();
        assert_eq!(m.index.metric.as_deref(), Some("l2"));
        assert!(
            m.notes.len() == 1 && m.notes[0].contains("hamming queda euclidean"),
            "{:?}",
            m.notes
        );
        let f32_col = L::Vector {
            dims: Some(8),
            element: dbine_driver::VectorElement::F32,
            sparse: false,
        };
        assert!(d.vector_index_pair(&hamming, Some(&f32_col)).is_err());
        // Couchbase's squared euclidean becomes euclidean, said.
        let sq = VectorIndex {
            algorithm: Some("diskANN".into()),
            metric: Some("l2_squared".into()),
            ..Default::default()
        };
        let m = d.vector_index(&sq, &ctx(None)).unwrap();
        assert_eq!(
            (m.index.algorithm.as_deref(), m.index.metric.as_deref()),
            (Some("diskANN"), Some("l2"))
        );
        assert!(
            m.notes[0].contains("l2_squared queda euclidean"),
            "{:?}",
            m.notes
        );
        // A flat index past 505 dimensions becomes quantizedFlat, said.
        let flat = VectorIndex {
            algorithm: Some("flat".into()),
            ..Default::default()
        };
        let f32_of = |n| L::Vector {
            dims: Some(n),
            element: dbine_driver::VectorElement::F32,
            sparse: false,
        };
        let (wide, narrow) = (f32_of(1536), f32_of(505));
        let m = d.vector_index(&flat, &ctx(Some(&wide))).unwrap();
        assert_eq!(m.index.algorithm.as_deref(), Some("quantizedFlat"));
        assert!(
            m.notes.len() == 1
                && m.notes[0].contains("quantizedFlat")
                && m.notes[0].contains("1536"),
            "{:?}",
            m.notes
        );
        let m = d.vector_index(&flat, &ctx(Some(&narrow))).unwrap();
        assert_eq!(m.index.algorithm.as_deref(), Some("flat"));
        assert!(m.notes.is_empty());
        // The column it searches: the embedding policy needs its dimension.
        let col = |dims| L::Vector {
            dims,
            element: dbine_driver::VectorElement::F32,
            sparse: false,
        };
        assert!(d.vector_index_pair(&disk, Some(&col(Some(1536)))).is_ok());
        let why = d.vector_index_pair(&disk, Some(&col(None))).unwrap_err();
        assert!(why.contains("no tiene dimensión"), "{why}");
        // Up to 4096 dimensions (an Oracle VECTOR(8192) is dropped).
        assert!(d.vector_index_pair(&disk, Some(&col(Some(4096)))).is_ok());
        let why = d
            .vector_index_pair(&disk, Some(&col(Some(8192))))
            .unwrap_err();
        assert!(why.contains("hasta 4096 dimensiones"), "{why}");
    }

    #[test]
    fn vector_indexes_survive_the_conversion() {
        use dbine_driver::{ColumnDef, IndexDef, KeyDef, VectorIndex};
        // A pgvector table: a vector searched by an HNSW index, and one
        // with a filter column (as MongoDB's take).
        let col = |n: &str, t: &str| ColumnDef {
            name: n.into(),
            data_type: t.into(),
            nullable: n != "id",
            ..Default::default()
        };
        let vix = |n: &str, cols: &[&str]| IndexDef {
            name: n.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            kind: Some("hnsw".into()),
            vector: Some(VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some("l2".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let t = TableSchema {
            schema: Some("public".into()),
            name: "docs".into(),
            columns: vec![
                col("id", "integer"),
                col("cat", "text"),
                col("emb", "vector(3)"),
                col("e2", "vector(3)"),
            ],
            primary_key: Some(KeyDef {
                columns: vec!["id".into()],
                ..Default::default()
            }),
            indexes: vec![vix("emb_l2", &["emb"]), vix("e2_l2", &["e2", "cat"])],
            ..Default::default()
        };
        let c = crate::convert::convert(&[t], "postgres", "cosmosdb", &Default::default()).unwrap();
        let out = &c.tables[0];
        // Kept as vector indexes (not dropped as single-field ones, not
        // made composite), one path each.
        assert_eq!(out.indexes.len(), 2, "{:?}", c.issues);
        for ix in &out.indexes {
            assert_eq!(ix.columns.len(), 1, "{ix:?}");
            assert_ne!(ix.kind.as_deref(), Some("composite"), "{ix:?}");
            assert_eq!(ix.vector.as_ref().unwrap().metric.as_deref(), Some("l2"));
        }
        assert!(c
            .issues
            .iter()
            .any(|i| i.code == IssueCode::IndexChanged && i.message.contains("quedan fuera cat")));
    }
}
