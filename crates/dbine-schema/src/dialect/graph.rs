//! Graph engines: Neo4j, Memgraph and Neptune (Cypher), and OrientDB.
//!
//! Cypher engines: a table is a node label and its columns are the nodes'
//! properties. `database_schema` reports each label (and relationship
//! type) with the property types of a sample (`STRING`, `INTEGER`,
//! `FLOAT`, `BOOLEAN`, `LIST`, `MAP`, `POINT`, several joined with `|`)
//! and its indexes and constraints. Nothing declares a property's type, so
//! the rendered types only document the values; what reaches the target
//! is the indexes: the primary key becomes a uniqueness constraint, unique
//! indexes too, the rest range indexes. Properties can't hold maps, so
//! JSON is text. Foreign keys aren't turned into relationships at this
//! stage: they're dropped and reported. Neptune has no user-defined
//! indexes or constraints, so it receives nothing but the report.
//!
//! Embeddings are lists of FLOAT (Neo4j 2025's `VECTOR<FLOAT32>(n)` reads
//! as a vector too); the dimension and distance belong to the vector index
//! (Neo4j `vector.dimensions` / `vector.similarity_function`, Memgraph
//! `dimension` / `metric`), which another family's vector index becomes,
//! with the dimension of the column it searches. As a source, a
//! `LIST<FLOAT>` property is a list (the type alone doesn't say it's an
//! embedding); one a vector index searches moves as a vector of the
//! dimension the index states (`convert`), so targets that keep the
//! dimension in the column (Spanner's `vector_length`, Snowflake's
//! `VECTOR(FLOAT, n)`, pgvector's `vector(n)`) get it.
//!
//! OrientDB: a table is a (document) class with a `CREATE PROPERTY` per
//! column, NOT NULL, defaults and indexes, the primary key as a unique
//! index. Its links are record ids, not the values a foreign key holds, so
//! foreign keys are dropped.

use super::mongodb::parse_list;
use super::{Caps, Dialect, IdentCase, Rendered, ALL_ACTIONS};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{IndexDef, TableSchema};

#[derive(Clone, Copy, PartialEq)]
enum Flavor {
    Neo4j,
    Memgraph,
    Neptune,
}

pub struct Cypher {
    flavor: Flavor,
}

pub struct OrientDb;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static NEO4J: Cypher = Cypher {
        flavor: Flavor::Neo4j,
    };
    static MEMGRAPH: Cypher = Cypher {
        flavor: Flavor::Memgraph,
    };
    static NEPTUNE: Cypher = Cypher {
        flavor: Flavor::Neptune,
    };
    static ORIENT: OrientDb = OrientDb;
    match driver_id {
        "neo4j" => Some(&NEO4J),
        "memgraph" => Some(&MEMGRAPH),
        "neptune" => Some(&NEPTUNE),
        "orientdb" => Some(&ORIENT),
        _ => None,
    }
}

// ---- Cypher -----------------------------------------------------------------

/// Neo4j 2025's `vector<float32>(3)` (lower case, as `type_list` gives it).
fn cypher_vector(t: &str) -> Option<L> {
    use dbine_driver::VectorElement as E;
    let rest = t.strip_prefix("vector")?.trim_start();
    let (element, rest) = match rest.strip_prefix('<') {
        Some(r) => {
            let (e, r) = r.split_once('>')?;
            (
                // valueType() adds ` not null` to the coordinate type.
                match e.trim().trim_end_matches(" not null").trim() {
                    "float32" => E::F32,
                    "float" | "float64" => E::F64,
                    "int8" | "integer8" => E::I8,
                    _ => E::Other,
                },
                r.trim(),
            )
        }
        None => (E::F32, rest),
    };
    let dims = match rest {
        "" => None,
        r => Some(
            r.strip_prefix('(')?
                .strip_suffix(')')?
                .trim()
                .parse()
                .ok()?,
        ),
    };
    Some(L::Vector {
        dims,
        element,
        sparse: false,
    })
}

fn cypher_type(t: &str) -> L {
    let t = t.trim();
    if let Some(v) = cypher_vector(t) {
        return v;
    }
    if let Some(inner) = t.strip_prefix("list<").and_then(|r| r.strip_suffix('>')) {
        return L::Array {
            of: Box::new(cypher_type(inner)),
        };
    }
    match t {
        "boolean" => L::Bool,
        "integer" => L::int(8),
        "float" => L::Float { bytes: 8 },
        "string" => L::Text { unicode: true },
        "date" => L::Date,
        "local time" => L::Time {
            precision: Some(9),
            tz: false,
        },
        "zoned time" => L::Time {
            precision: Some(9),
            tz: true,
        },
        "local datetime" => L::Timestamp {
            precision: Some(9),
            tz: false,
        },
        "zoned datetime" => L::Timestamp {
            precision: Some(9),
            tz: true,
        },
        "duration" => L::Interval,
        "point" => L::Geometry {
            kind: Some("point".into()),
            srid: None,
            geography: false,
        },
        "list" | "map" | "any" => L::Json { binary: true },
        _ => L::Other {
            native: t.to_string(),
        },
    }
}

/// A vector index's `quantization` as a Memgraph `scalar_kind` (`f32`
/// is none), `None` when Memgraph has no counterpart.
fn memgraph_scalar_kind(q: &str) -> Option<&'static str> {
    Some(match q.trim().to_ascii_lowercase().as_str() {
        "" | "none" | "false" | "off" | "f32" | "float32" | "fp32" => "f32",
        "f16" | "float16" | "fp16" | "half" | "halfvec" => "f16",
        "bf16" | "bfloat16" => "bf16",
        "f64" | "float64" => "f64",
        "i8" | "int8" | "sq8" | "scalar" => "i8",
        "b1" | "b1x8" | "bit" | "binary" => "b1x8",
        _ => return None,
    })
}

impl Cypher {
    /// Neo4j, Memgraph and Neptune are one family (one [`Dialect::id`]),
    /// so `convert` hands their vector indexes from one to another as they
    /// are: each is fitted to this flavor here, as `vector_index` does
    /// for other families (Memgraph's required dimension, from the
    /// property's `VECTOR<FLOAT32>(3)` when the index doesn't say it;
    /// Neo4j's similarities and limits), or left out, said, when it can't
    /// be made. An index copied within one flavor, or one `vector_index`
    /// already fitted, stays as it is.
    fn fit_vector_indexes(&self, t: &mut TableSchema, report: &mut Report) {
        let name = t.name.clone();
        let columns = &t.columns;
        t.indexes.retain_mut(|ix| {
            let Some(v) = &ix.vector else {
                return true;
            };
            // Only Memgraph needs the column (Neo4j's index may leave the
            // dimension out, and a copy between Neo4j databases keeps it
            // that way).
            let column = ix
                .columns
                .first()
                .filter(|_| self.flavor == Flavor::Memgraph)
                .and_then(|c| columns.iter().find(|x| &x.name == c))
                .map(|c| self.parse_type(&crate::parse::parse(&c.data_type)));
            match self.vector_index(v, column.as_ref()) {
                Ok((kind, v)) => {
                    if kind.is_some() {
                        ix.kind = kind;
                    }
                    ix.vector = Some(v);
                    true
                }
                Err(why) => {
                    report.push(
                        Severity::Dropped,
                        IssueCode::IndexDropped,
                        &name,
                        Some(&ix.name),
                        format!("El índice vectorial «{}» no se crea: {why}.", ix.name),
                    );
                    false
                }
            }
        });
    }

    fn render(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Vector { sparse: true, .. } => super::vector_fallback(self, t),
            // Neptune has no vector indexes to give it its dimension.
            L::Vector { .. } if self.flavor == Flavor::Neptune => super::vector_fallback(self, t),
            // What vector indexes index: a list of floats (the native VECTOR
            // of Neo4j 2025 needs Bolt 6). The dimension is the index's.
            L::Vector { dims, .. } => Rendered::exact("LIST<FLOAT>").with(
                Info,
                TypeChanged,
                match dims {
                    Some(n) => format!("El vector queda como lista de FLOAT: la propiedad no fija su dimensión ({n}); la fija el índice vectorial."),
                    None => "El vector queda como lista de FLOAT.".to_string(),
                },
            ),
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } if L::signed_bytes_for(*bytes, *unsigned) <= 8 => Rendered::exact("INTEGER"),
            L::Int { .. } => Rendered::exact("FLOAT").with(Loss, RangeLoss, "Los enteros son de 8 bytes: el valor queda como FLOAT y pierde precisión pasados los 15 dígitos."),
            L::Decimal { precision: Some(p), .. } if *p <= 15 => Rendered::exact("FLOAT").with(Info, TypeChanged, "No hay decimales exactos: FLOAT, exacto hasta 15 dígitos."),
            L::Decimal { .. } => Rendered::exact("FLOAT").with(Loss, PrecisionLoss, "No hay decimales exactos: FLOAT pierde precisión pasados los 15 dígitos."),
            L::Float { .. } => Rendered::exact("FLOAT"),
            L::Money => Rendered::exact("FLOAT").with(Info, TypeChanged, "Moneda como FLOAT."),
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } | L::Uuid | L::Inet | L::MacAddr => Rendered::exact("STRING"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("LIST<INTEGER>").with(Info, TypeChanged, "Binario como arreglo de bytes."),
            L::Bit { .. } => Rendered::exact("STRING").with(Warning, TypeApproximated, "Cadena de bits como texto de 0 y 1."),
            L::Date => Rendered::exact("DATE"),
            L::Time { tz, .. } => Rendered::exact(if *tz { "ZONED TIME" } else { "LOCAL TIME" }),
            L::Timestamp { tz, .. } => Rendered::exact(if *tz { "ZONED DATETIME" } else { "LOCAL DATETIME" }),
            L::Interval => Rendered::exact("DURATION"),
            L::Year => Rendered::exact("INTEGER"),
            L::Json { .. } | L::Map { .. } => Rendered::exact("STRING").with(Warning, TypeApproximated, "Las propiedades no guardan mapas: el JSON queda como texto."),
            L::Xml => Rendered::exact("STRING"),
            L::Enum { values } => Rendered::exact("STRING").with(Info, TypeApproximated, format!("Enumerado como texto. Valores: {}.", values.join(", "))),
            L::Set { .. } => Rendered::exact("LIST<STRING>"),
            L::Array { of } => match **of {
                L::Array { .. } | L::Json { .. } | L::Map { .. } => {
                    Rendered::exact("STRING").with(Warning, TypeApproximated, "Las propiedades no guardan listas anidadas: queda como texto JSON.")
                }
                _ => {
                    let inner = self.render(of);
                    Rendered { native: format!("LIST<{}>", inner.native), notes: inner.notes }
                }
            },
            L::Geometry { kind: Some(k), .. } if k == "point" => Rendered::exact("POINT"),
            L::Geometry { .. } => Rendered::exact("STRING").with(Warning, TypeApproximated, "Solo hay puntos espaciales: la geometría queda como texto (WKT)."),
            L::RowVersion => Rendered::exact("STRING").with(Warning, TypeApproximated, "No hay versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }
}

impl Dialect for Cypher {
    fn id(&self) -> &'static str {
        "neo4j"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let raw = t.raw.trim();
        if raw.is_empty() {
            return L::Json { binary: true };
        }
        match parse_list(raw, cypher_type) {
            L::Other { .. } => L::Other {
                native: raw.to_string(),
            },
            l => l,
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        self.render(t)
    }

    fn render_default(&self, _d: &DefaultValue, _ty: &L) -> Option<String> {
        None
    }

    fn caps(&self) -> Caps {
        Caps {
            // Let foreign keys through to `finalize`, which reports them as
            // relationships not created (and drops them).
            foreign_keys: true,
            on_delete: ALL_ACTIONS,
            on_update: ALL_ACTIONS,
            indexes: self.flavor != Flavor::Neptune,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: false,
            nullability: false,
            comments: false,
            max_identifier: 255,
            case: IdentCase::Preserve,
            vector_indexes: self.flavor != Flavor::Neptune,
            vectors: self.flavor != Flavor::Neptune,
        }
    }

    /// Neo4j: HNSW with cosine or euclidean similarity, `m`,
    /// `ef_construction`, `quantization` and `dimensions`, within Neo4j's
    /// limits (the driver checks them too, but there a value out of range
    /// fails the label's whole script). Memgraph: cosine, l2 (as l2sq),
    /// ip, hamming and its own metrics, with `dimensions` (required),
    /// `capacity`, `resize_coefficient` and `quantization` (its
    /// `scalar_kind`). The dimension is the searched column's when the
    /// source index doesn't state it (other families keep it in the
    /// column's type: `vector(3)`, `vector_length=>3`). A sparse vector is
    /// text here, so it has no vector index. Neptune has no indexes of its
    /// own.
    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
        column: Option<&crate::logical::LogicalType>,
    ) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
        use dbine_driver::vector::{metric, param};
        let engine = match self.flavor {
            Flavor::Neptune => {
                return Err("Neptune no tiene índices definidos por el usuario".into())
            }
            Flavor::Neo4j => "Neo4j",
            Flavor::Memgraph => "Memgraph",
        };
        let mut v = v.clone();
        match column {
            Some(L::Vector { sparse: true, .. }) => {
                return Err(format!(
                    "{engine} lo hace solo sobre listas de números y un vector disperso queda como texto"
                ))
            }
            Some(L::Vector { dims: Some(n), .. }) => {
                v.params
                    .entry("dimensions".into())
                    .or_insert_with(|| n.to_string());
            }
            _ => {}
        }
        let keep = |keys: &[&str]| {
            v.params
                .iter()
                .filter(|(k, _)| keys.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let out = match self.flavor {
            Flavor::Neptune => unreachable!(),
            Flavor::Neo4j => {
                let m = match v.metric.as_deref() {
                    None => None,
                    Some(m @ (metric::COSINE | metric::L2)) => Some(m.to_string()),
                    Some(m) => {
                        return Err(format!(
                            "Neo4j no tiene la similitud «{m}» (solo coseno y euclídea)"
                        ))
                    }
                };
                for (k, hi) in [
                    ("dimensions", 4096),
                    (param::M, 512),
                    (param::EF_CONSTRUCTION, 3200),
                ] {
                    if let Some(x) = v.params.get(k).map(|x| x.trim()) {
                        if !x.parse::<u32>().is_ok_and(|n| (1..=hi).contains(&n)) {
                            return Err(format!("en Neo4j, {k} va de 1 a {hi}, no {x}"));
                        }
                    }
                }
                dbine_driver::VectorIndex {
                    algorithm: Some("hnsw".into()),
                    metric: m,
                    params: keep(&[
                        param::M,
                        param::EF_CONSTRUCTION,
                        param::QUANTIZATION,
                        "dimensions",
                    ]),
                }
            }
            Flavor::Memgraph => {
                let m = match v.metric.as_deref() {
                    None => None,
                    Some(m @ (metric::COSINE | metric::L2 | metric::IP | metric::HAMMING)) => {
                        Some(m.to_string())
                    }
                    Some(
                        m @ ("pearson" | "haversine" | "divergence" | "tanimoto" | "sorensen"),
                    ) => Some(m.to_string()),
                    Some(m) => return Err(format!("Memgraph no tiene la distancia «{m}»")),
                };
                if !v.params.contains_key("dimensions") {
                    return Err(
                        "Memgraph necesita la dimensión en el índice y ni el índice ni la columna de origen la dicen; crealo a mano con su dimension"
                            .into(),
                    );
                }
                let mut params = keep(&["dimensions", "capacity", "resize_coefficient"]);
                if let Some(q) = v.params.get(param::QUANTIZATION) {
                    match memgraph_scalar_kind(q) {
                        Some("f32") => {}
                        Some(k) => {
                            params.insert(param::QUANTIZATION.into(), k.into());
                        }
                        None => {
                            return Err(format!(
                                "Memgraph no tiene la cuantización «{}» (solo f16, bf16, f64, i8 y b1x8)",
                                q.trim()
                            ))
                        }
                    }
                }
                dbine_driver::VectorIndex {
                    algorithm: None,
                    metric: m,
                    params,
                }
            }
        };
        Ok((Some("VECTOR".into()), out))
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        self.fit_vector_indexes(t, report);
        if t.kind == "label" || t.kind == "relationship" {
            return;
        }
        let name = t.name.clone();
        t.kind = "label".into();
        report.push(
            Severity::Info,
            IssueCode::TableChanged,
            &name,
            None,
            "La tabla pasa a ser una etiqueta de nodos y sus columnas, propiedades: los tipos indican cómo quedan los valores, nada los controla.",
        );
        for fk in std::mem::take(&mut t.foreign_keys) {
            let label = fk
                .name
                .clone()
                .unwrap_or_else(|| format!("→ {}", fk.ref_table));
            report.push(
                Severity::Dropped,
                IssueCode::ForeignKeyDropped,
                &name,
                Some(&label),
                format!(
                    "La clave foránea hacia «{}» no se convierte en relación: las propiedades ({}) quedan con sus valores y la relación se puede crear después con MATCH … CREATE.",
                    fk.ref_table,
                    fk.columns.join(", ")
                ),
            );
        }
        let pk = t.primary_key.take().filter(|k| !k.columns.is_empty());
        if self.flavor == Flavor::Neptune {
            if pk.is_some() {
                report.push(
                    Severity::Info,
                    IssueCode::PrimaryKeyDropped,
                    &name,
                    None,
                    "Neptune no tiene restricciones: la clave primaria no se controla.",
                );
            }
            return;
        }
        if let Some(pk) = pk {
            if !t
                .indexes
                .iter()
                .any(|i| i.unique && i.columns == pk.columns)
            {
                let ix = pk.name.clone().unwrap_or_else(|| format!("{name}_pk"));
                report.push(
                    Severity::Info,
                    IssueCode::PrimaryKeyDropped,
                    &name,
                    Some(&ix),
                    format!("La clave primaria ({}) pasa a una restricción de unicidad; que no sea nula no se controla.", pk.columns.join(", ")),
                );
                t.indexes.insert(
                    0,
                    IndexDef {
                        name: ix,
                        columns: pk.columns,
                        unique: true,
                        kind: Some("UNIQUE".into()),
                        filter: None,
                        ..Default::default()
                    },
                );
            }
        }
        for ix in &mut t.indexes {
            if ix.kind.is_none() {
                ix.kind = Some(if ix.unique { "UNIQUE" } else { "RANGE" }.into());
            }
        }
    }
}

// ---- OrientDB ---------------------------------------------------------------

fn orient_type(t: &str) -> L {
    match t {
        "boolean" => L::Bool,
        "byte" => L::int(1),
        "short" => L::int(2),
        "integer" => L::int(4),
        "long" => L::int(8),
        "float" => L::Float { bytes: 4 },
        "double" => L::Float { bytes: 8 },
        "decimal" => L::Decimal {
            precision: None,
            scale: None,
        },
        "string" => L::Text { unicode: true },
        "date" => L::Date,
        // Milliseconds since the epoch, shown in the database's time zone.
        "datetime" => L::Timestamp {
            precision: Some(3),
            tz: true,
        },
        "binary" | "custom" => L::Blob,
        // A record id (`#12:3`).
        "link" => L::Varchar {
            len: Some(32),
            unicode: false,
        },
        "embedded" | "embeddedlist" | "embeddedset" | "embeddedmap" | "linklist" | "linkset"
        | "linkmap" | "linkbag" | "any" => L::Json { binary: true },
        _ => L::Other {
            native: t.to_string(),
        },
    }
}

impl Dialect for OrientDb {
    fn id(&self) -> &'static str {
        "orientdb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let raw = t.raw.trim();
        if raw.is_empty() {
            return L::Json { binary: true };
        }
        match parse_list(raw, orient_type) {
            L::Other { .. } => L::Other {
                native: raw.to_string(),
            },
            l => l,
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Vector { .. } => super::vector_fallback(self, t),
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => {
                let r = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 => Rendered::exact("BYTE"),
                    2 => Rendered::exact("SHORT"),
                    3 | 4 => Rendered::exact("INTEGER"),
                    8 => Rendered::exact("LONG"),
                    _ => Rendered::exact("DECIMAL"),
                };
                if *unsigned {
                    r.with(
                        Info,
                        TypeChanged,
                        "OrientDB no tiene enteros sin signo: se usa un tipo más grande con signo.",
                    )
                } else {
                    r
                }
            }
            L::Decimal { .. } | L::Money => Rendered::exact("DECIMAL"),
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Char { len: Some(_), .. } | L::Varchar { len: Some(_), .. } => {
                Rendered::exact("STRING").with(Info, TypeChanged, "STRING no limita el largo.")
            }
            L::Char { .. }
            | L::Varchar { .. }
            | L::Text { .. }
            | L::Uuid
            | L::Xml
            | L::Inet
            | L::MacAddr => Rendered::exact("STRING"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("BINARY"),
            L::Bit { .. } => Rendered::exact("STRING").with(
                Warning,
                TypeApproximated,
                "Cadena de bits como texto de 0 y 1.",
            ),
            L::Date => Rendered::exact("DATE"),
            L::Time { .. } => Rendered::exact("STRING").with(
                Warning,
                TypeApproximated,
                "OrientDB no tiene hora del día: queda como texto.",
            ),
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact("DATETIME");
                let r = match precision {
                    Some(p) if *p > 3 => r.with(
                        Loss,
                        PrecisionLoss,
                        format!(
                            "DATETIME guarda milisegundos: se pierden {} decimales de segundo.",
                            p - 3
                        ),
                    ),
                    _ => r,
                };
                if *tz {
                    r
                } else {
                    r.with(Warning, TimeZoneLoss, "DATETIME es un instante: la fecha y hora sin zona se toma en la zona horaria de la base.")
                }
            }
            L::Interval => Rendered::exact("STRING").with(
                Warning,
                TypeApproximated,
                "OrientDB no tiene intervalos: queda como texto.",
            ),
            L::Year => Rendered::exact("SHORT").with(Info, TypeChanged, "Año como SHORT."),
            L::Json { .. } => {
                Rendered::exact("EMBEDDED").with(Info, TypeChanged, "JSON como documento embebido.")
            }
            L::Enum { values } => Rendered::exact("STRING").with(
                Info,
                TypeApproximated,
                format!("Enumerado como texto. Valores: {}.", values.join(", ")),
            ),
            L::Set { .. } => Rendered::exact("EMBEDDEDSET"),
            L::Array { .. } => Rendered::exact("EMBEDDEDLIST"),
            L::Map { .. } => Rendered::exact("EMBEDDEDMAP"),
            L::Geometry { .. } => Rendered::exact("STRING").with(
                Warning,
                TypeApproximated,
                "Dato espacial como texto (WKT).",
            ),
            L::RowVersion => Rendered::exact("BINARY").with(
                Warning,
                TypeApproximated,
                "OrientDB no tiene versión de fila automática: no se actualiza sola.",
            ),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    /// The driver quotes the default; OrientDB converts it to the
    /// property's type and evaluates functions such as `sysdate()`.
    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => s.clone(),
            DefaultValue::Bool(b) => b.to_string(),
            DefaultValue::CurrentTimestamp | DefaultValue::CurrentDate
                if matches!(ty, L::Date | L::Timestamp { .. }) =>
            {
                "sysdate()".into()
            }
            _ => return None,
        })
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: false,
            on_delete: &[],
            on_update: &[],
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: true,
            nullability: true,
            comments: false,
            max_identifier: 128,
            case: IdentCase::Preserve,
            // OrientDB 3.2 has no vector indexes.
            vector_indexes: false,
            vectors: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn vectors_are_float_lists_with_the_dimension_in_the_index() {
        use dbine_driver::{VectorElement as E, VectorIndex as V};
        let d = lookup("neo4j").unwrap();
        let p = |s: &str| d.parse_type(&parse(s));
        assert_eq!(
            p("VECTOR<FLOAT32>(3)"),
            L::Vector {
                dims: Some(3),
                element: E::F32,
                sparse: false
            }
        );
        assert_eq!(
            p("VECTOR<FLOAT32 NOT NULL>(3)"),
            L::Vector {
                dims: Some(3),
                element: E::F32,
                sparse: false
            }
        );
        assert_eq!(
            p("VECTOR<INTEGER8>(1024)"),
            L::Vector {
                dims: Some(1024),
                element: E::I8,
                sparse: false
            }
        );
        let r = d.render_type(&L::Vector {
            dims: Some(3),
            element: E::F32,
            sparse: false,
        });
        assert_eq!(r.native, "LIST<FLOAT>");
        assert_eq!(r.notes[0].severity, Severity::Info);
        let v = |alg: Option<&str>, m: Option<&str>, ps: &[(&str, &str)]| V {
            algorithm: alg.map(str::to_string),
            metric: m.map(str::to_string),
            params: ps
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        // pgvector's HNSW on Neo4j: its settings, under Neo4j's names later.
        let (kind, got) = d
            .vector_index(
                &v(
                    Some("hnsw"),
                    Some("cosine"),
                    &[("m", "24"), ("ef_construction", "200")],
                ),
                None,
            )
            .unwrap();
        assert_eq!(kind.as_deref(), Some("VECTOR"));
        assert_eq!(
            got,
            v(
                Some("hnsw"),
                Some("cosine"),
                &[("ef_construction", "200"), ("m", "24")]
            )
        );
        let (_, got) = d
            .vector_index(&v(Some("ivfflat"), Some("l2"), &[("lists", "100")]), None)
            .unwrap();
        assert_eq!(got, v(Some("hnsw"), Some("l2"), &[]));
        assert!(d.vector_index(&v(None, Some("ip"), &[]), None).is_err());
        let mg = lookup("memgraph").unwrap();
        assert!(mg
            .vector_index(&v(None, Some("cosine"), &[]), None)
            .unwrap_err()
            .contains("dimensión"));
        // The dimension from the searched column (pgvector's vector(3)),
        // when the index doesn't state it; the index's own wins.
        let col = L::Vector {
            dims: Some(3),
            element: E::F32,
            sparse: false,
        };
        let (_, got) = mg
            .vector_index(&v(Some("hnsw"), Some("cosine"), &[]), Some(&col))
            .unwrap();
        assert_eq!(got, v(None, Some("cosine"), &[("dimensions", "3")]));
        let (_, got) = d
            .vector_index(&v(Some("hnsw"), Some("cosine"), &[]), Some(&col))
            .unwrap();
        assert_eq!(got, v(Some("hnsw"), Some("cosine"), &[("dimensions", "3")]));
        let (_, got) = d
            .vector_index(&v(None, Some("cosine"), &[("dimensions", "4")]), Some(&col))
            .unwrap();
        assert_eq!(got.params["dimensions"], "4");
        // A column without a dimension (a list) gives none.
        let list = L::Array {
            of: Box::new(L::Float { bytes: 4 }),
        };
        assert!(mg
            .vector_index(&v(None, Some("cosine"), &[]), Some(&list))
            .is_err());
        let (_, got) = mg
            .vector_index(
                &v(None, Some("ip"), &[("dimensions", "3"), ("m", "16")]),
                None,
            )
            .unwrap();
        assert_eq!(got, v(None, Some("ip"), &[("dimensions", "3")]));
        assert!(lookup("neptune")
            .unwrap()
            .vector_index(&v(None, None, &[]), None)
            .is_err());
    }

    /// A label with an embedding and its vector index, as `database_schema`
    /// reads it, converted to the families that keep the dimension in the
    /// column (Spanner, Snowflake, pgvector), to BigQuery (none): the
    /// searched list moves as a vector of the index's dimension, the plain
    /// list stays a list everywhere.
    #[test]
    fn graph_embeddings_as_a_source() {
        use dbine_driver::{ColumnDef, IndexDef, VectorIndex};
        for from in ["neo4j", "memgraph"] {
            let mut v = VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some("cosine".into()),
                ..Default::default()
            };
            v.params.insert("dimensions".into(), "3".into());
            let t = TableSchema {
                kind: "label".into(),
                name: "Doc".into(),
                columns: vec![
                    ColumnDef {
                        name: "emb".into(),
                        data_type: "LIST<FLOAT>".into(),
                        nullable: true,
                        ..Default::default()
                    },
                    ColumnDef {
                        name: "prices".into(),
                        data_type: "LIST<FLOAT>".into(),
                        nullable: true,
                        ..Default::default()
                    },
                ],
                indexes: vec![IndexDef {
                    name: "doc_emb".into(),
                    columns: vec!["emb".into()],
                    kind: Some("VECTOR".into()),
                    vector: Some(v),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let conv = |to: &str| {
                crate::convert(std::slice::from_ref(&t), from, to, &Default::default()).unwrap()
            };
            let ty = |c: &crate::Conversion, n: &str| {
                c.tables[0]
                    .columns
                    .iter()
                    .find(|x| x.name.eq_ignore_ascii_case(n))
                    .unwrap()
                    .data_type
                    .clone()
            };
            let sp = conv("spanner");
            assert_eq!(ty(&sp, "emb"), "ARRAY<FLOAT64>(vector_length=>3)", "{from}");
            assert_eq!(ty(&sp, "prices"), "ARRAY<FLOAT64>");
            assert!(sp.tables[0].indexes.iter().any(|i| i.vector.is_some()));
            let sf = conv("snowflake");
            assert_eq!(ty(&sf, "emb"), "VECTOR(FLOAT, 3)");
            assert_eq!(ty(&sf, "prices"), "ARRAY");
            let bq = conv("bigquery");
            assert_eq!(ty(&bq, "emb"), "ARRAY<FLOAT64>");
            let ix = bq.tables[0]
                .indexes
                .iter()
                .find(|i| i.vector.is_some())
                .unwrap();
            assert_eq!(
                ix.vector.as_ref().unwrap().metric.as_deref(),
                Some("cosine")
            );
            let pg = conv("postgres");
            assert_eq!(ty(&pg, "prices"), "double precision[]");
            assert_eq!(ty(&pg, "emb"), "vector(3)");
            let ix = pg.tables[0]
                .indexes
                .iter()
                .find(|i| i.vector.is_some())
                .unwrap();
            assert_eq!(ix.columns, ["emb"]);
            // Without a dimension in the index the list stays a list, and
            // its vector index is left out (said).
            let mut bare = t.clone();
            bare.indexes[0].vector.as_mut().unwrap().params.clear();
            let pg = crate::convert(&[bare], from, "postgres", &Default::default()).unwrap();
            assert_eq!(pg.tables[0].columns[0].data_type, "double precision[]");
            assert!(pg.tables[0].indexes.is_empty());
            assert!(pg.issues.iter().any(|i| i.code == IssueCode::IndexDropped));
        }
    }

    /// The family's engines without vector indexes say so: a vector
    /// index from pgvector is left out with that reason, not with a
    /// reason about its column.
    #[test]
    fn engines_without_vector_indexes_say_so() {
        use dbine_driver::{ColumnDef, IndexDef, VectorIndex};
        let t = TableSchema {
            name: "docs".into(),
            columns: vec![ColumnDef {
                name: "emb".into(),
                data_type: "vector(3)".into(),
                nullable: true,
                ..Default::default()
            }],
            indexes: vec![IndexDef {
                name: "docs_emb".into(),
                columns: vec!["emb".into()],
                kind: Some("hnsw".into()),
                vector: Some(VectorIndex {
                    algorithm: Some("hnsw".into()),
                    metric: Some("cosine".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        // Every id of each dialect, the ones sharing a struct included.
        for to in [
            "orientdb",
            "neptune",
            "trino",
            "starburst",
            "presto",
            "athena",
            "hive",
            "cloudera",
            "impala",
            "spark",
            "kyuubi",
            "databricks",
            "azure_databricks",
            "dremio",
            "drill",
            "iotdb",
            "timechodb",
            "tdengine",
            "influxdb",
            "influxdb1",
            "influxdb3",
            "ksqldb",
            "phoenix",
            "avatica",
            "kdb",
            "powerbi",
        ] {
            let d = crate::dialect::for_driver(to).unwrap();
            assert!(!d.caps().vector_indexes, "{to}");
            assert!(
                d.vector_index(&VectorIndex::default(), None).is_err(),
                "{to}"
            );
            // Drill, InfluxDB, a generic Avatica server and Power BI take
            // no tables made elsewhere.
            let Ok(c) = crate::convert(
                std::slice::from_ref(&t),
                "postgres",
                to,
                &Default::default(),
            ) else {
                assert!(
                    [
                        "drill",
                        "influxdb",
                        "influxdb1",
                        "influxdb3",
                        "avatica",
                        "powerbi"
                    ]
                    .contains(&to),
                    "{to}"
                );
                continue;
            };
            assert!(
                c.tables[0].indexes.iter().all(|i| i.vector.is_none()),
                "{to}"
            );
            assert!(
                c.issues.iter().any(|i| i.code == IssueCode::IndexDropped),
                "{to}"
            );
        }
        // Neptune leaves out every index (it has none of its own).
        let neptune = crate::convert(
            std::slice::from_ref(&t),
            "postgres",
            "neptune",
            &Default::default(),
        )
        .unwrap();
        assert!(neptune.tables[0].indexes.is_empty());
        let ok = crate::convert(&[t], "postgres", "kdb", &Default::default()).unwrap();
        assert!(ok.issues.iter().any(|i| i.code == IssueCode::IndexDropped
            && i.message.contains("no tiene índices vectoriales")));
    }

    /// pgvector's `vector(3)` with an HNSW index into Neo4j and Memgraph:
    /// the index takes the column's dimension (Memgraph requires it).
    #[test]
    fn vector_indexes_take_the_column_s_dimension() {
        use dbine_driver::{ColumnDef, IndexDef, VectorIndex};
        let t = TableSchema {
            name: "docs".into(),
            columns: vec![ColumnDef {
                name: "emb".into(),
                data_type: "vector(3)".into(),
                nullable: true,
                ..Default::default()
            }],
            indexes: vec![IndexDef {
                name: "docs_emb".into(),
                columns: vec!["emb".into()],
                kind: Some("hnsw".into()),
                vector: Some(VectorIndex {
                    algorithm: Some("hnsw".into()),
                    metric: Some("cosine".into()),
                    params: [("m".to_string(), "16".to_string())].into(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        for to in ["neo4j", "memgraph"] {
            let c = crate::convert(
                std::slice::from_ref(&t),
                "postgres",
                to,
                &Default::default(),
            )
            .unwrap();
            let ix = c.tables[0]
                .indexes
                .iter()
                .find(|i| i.vector.is_some())
                .unwrap_or_else(|| panic!("{to}: {:?}", c.issues));
            let v = ix.vector.as_ref().unwrap();
            assert_eq!(
                v.params.get("dimensions").map(String::as_str),
                Some("3"),
                "{to}"
            );
            assert_eq!(v.metric.as_deref(), Some("cosine"));
            assert_eq!(ix.kind.as_deref(), Some("VECTOR"));
            assert_eq!(c.tables[0].columns[0].data_type, "LIST<FLOAT>");
        }
    }

    /// A table with one vector column and an HNSW index on it, as another
    /// family's `database_schema` reads it.
    fn searched(data_type: &str, v: dbine_driver::VectorIndex) -> TableSchema {
        use dbine_driver::{ColumnDef, IndexDef};
        TableSchema {
            kind: "label".into(),
            name: "Doc".into(),
            columns: vec![ColumnDef {
                name: "emb".into(),
                data_type: data_type.into(),
                nullable: true,
                ..Default::default()
            }],
            indexes: vec![IndexDef {
                name: "doc_emb".into(),
                columns: vec!["emb".into()],
                kind: Some("VECTOR".into()),
                vector: Some(v),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn vix(metric: &str, ps: &[(&str, &str)]) -> dbine_driver::VectorIndex {
        dbine_driver::VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some(metric.into()),
            params: ps
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn dropped(c: &crate::Conversion, why: &str) -> bool {
        c.tables[0].indexes.iter().all(|i| i.vector.is_none())
            && c.issues.iter().any(|i| {
                i.code == IssueCode::IndexDropped
                    && i.object.as_deref() == Some("doc_emb")
                    && i.message.contains(why)
            })
    }

    /// pgvector's `sparsevec(5)` with HNSW: the column becomes text here, so
    /// its vector index is left out (said), on both engines.
    #[test]
    fn sparse_vectors_get_no_vector_index() {
        let mut t = searched("sparsevec(5)", vix("cosine", &[]));
        t.kind = "table".into();
        for to in ["neo4j", "memgraph"] {
            let c = crate::convert(
                std::slice::from_ref(&t),
                "postgres",
                to,
                &Default::default(),
            )
            .unwrap();
            assert_eq!(c.tables[0].columns[0].data_type, "STRING", "{to}");
            assert!(dropped(&c, "disperso"), "{to}: {:?}", c.issues);
            let sparse = L::Vector {
                dims: Some(5),
                element: dbine_driver::VectorElement::F32,
                sparse: true,
            };
            assert!(lookup(to)
                .unwrap()
                .vector_index(&vix("cosine", &[]), Some(&sparse))
                .is_err());
        }
    }

    /// Neo4j's limits (dimensions 1–4096, M 1–512, ef_construction 1–3200)
    /// leave the index out instead of failing the label's script; Memgraph
    /// has no such limit.
    #[test]
    fn neo4j_vector_limits_leave_the_index_out() {
        let mut t = searched("VECTOR(8192, FLOAT32)", vix("cosine", &[]));
        t.kind = "table".into();
        let neo = crate::convert(
            std::slice::from_ref(&t),
            "oracle",
            "neo4j",
            &Default::default(),
        )
        .unwrap();
        assert!(dropped(&neo, "4096"), "{:?}", neo.issues);
        let mg = crate::convert(
            std::slice::from_ref(&t),
            "oracle",
            "memgraph",
            &Default::default(),
        )
        .unwrap();
        let v = mg.tables[0].indexes[0].vector.as_ref().unwrap();
        assert_eq!(v.params["dimensions"], "8192");
        let d = lookup("neo4j").unwrap();
        for (k, bad, good) in [
            ("dimensions", "4097", "4096"),
            ("dimensions", "0", "1"),
            ("m", "513", "512"),
            ("ef_construction", "3201", "3200"),
            ("m", "x", "16"),
        ] {
            let err = d.vector_index(&vix("l2", &[(k, bad)]), None).unwrap_err();
            assert!(err.contains(k) && err.contains(bad), "{err}");
            let (_, v) = d.vector_index(&vix("l2", &[(k, good)]), None).unwrap();
            assert_eq!(v.params[k], good);
        }
    }

    /// Memgraph's `scalar_kind` from other engines' quantization names; one
    /// it has no counterpart for leaves the index out.
    #[test]
    fn memgraph_quantization() {
        let d = lookup("memgraph").unwrap();
        let q = |x: &str| {
            d.vector_index(
                &vix("cosine", &[("dimensions", "3"), ("quantization", x)]),
                None,
            )
            .map(|(_, v)| v.params.get("quantization").cloned())
        };
        assert_eq!(q("SQ8"), Ok(Some("i8".into())));
        assert_eq!(q("int8"), Ok(Some("i8".into())));
        assert_eq!(q("halfvec"), Ok(Some("f16".into())));
        assert_eq!(q("bf16"), Ok(Some("bf16".into())));
        assert_eq!(q("binary"), Ok(Some("b1x8".into())));
        assert_eq!(q("none"), Ok(None));
        assert_eq!(q("f32"), Ok(None));
        assert!(q("pq").unwrap_err().contains("«pq»"));
    }

    /// Neo4j and Memgraph share the dialect (one family), so `convert`
    /// hands a vector index over as it is and `finalize` fits it: Memgraph
    /// gets the dimension of a Neo4j 2025 `VECTOR<FLOAT32>(3)` property, or
    /// leaves the index out when nothing says it; Neo4j leaves out a
    /// Memgraph-only metric. Within one engine nothing changes.
    #[test]
    fn vector_indexes_between_cypher_engines() {
        let conv = |t: &TableSchema, from: &str, to: &str| {
            crate::convert(std::slice::from_ref(t), from, to, &Default::default()).unwrap()
        };
        let vector = |c: &crate::Conversion| c.tables[0].indexes[0].vector.clone().unwrap();
        // Neo4j (5.23+ leaves the dimension out) into Memgraph.
        let neo = searched("VECTOR<FLOAT32>(3)", vix("cosine", &[("m", "16")]));
        let mg = conv(&neo, "neo4j", "memgraph");
        assert_eq!(
            vector(&mg),
            dbine_driver::VectorIndex {
                algorithm: None,
                metric: Some("cosine".into()),
                params: [("dimensions".to_string(), "3".to_string())].into(),
            }
        );
        assert_eq!(mg.tables[0].indexes[0].kind.as_deref(), Some("VECTOR"));
        let list = searched("LIST<FLOAT>", vix("cosine", &[]));
        assert!(dropped(&conv(&list, "neo4j", "memgraph"), "dimensión"));
        // Neo4j into Neo4j: as it was, without a dimension.
        let same = conv(&neo, "neo4j", "neo4j");
        assert_eq!(vector(&same), neo.indexes[0].vector.clone().unwrap());
        // Memgraph into Neo4j: only cosine and euclidean, within its limits.
        let mut mgv = vix(
            "pearson",
            &[
                ("dimensions", "3"),
                ("capacity", "1000"),
                ("resize_coefficient", "2"),
                ("quantization", "i8"),
            ],
        );
        mgv.algorithm = None;
        let from_mg = searched("LIST<FLOAT>", mgv.clone());
        assert!(dropped(&conv(&from_mg, "memgraph", "neo4j"), "pearson"));
        let mut cos = from_mg.clone();
        cos.indexes[0].vector.as_mut().unwrap().metric = Some("cosine".into());
        let neo = conv(&cos, "memgraph", "neo4j");
        assert_eq!(
            vector(&neo),
            dbine_driver::VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some("cosine".into()),
                params: [
                    ("dimensions".to_string(), "3".to_string()),
                    ("quantization".to_string(), "i8".to_string()),
                ]
                .into(),
            }
        );
        let mut wide = cos.clone();
        wide.indexes[0]
            .vector
            .as_mut()
            .unwrap()
            .params
            .insert("dimensions".into(), "5000".into());
        assert!(dropped(&conv(&wide, "memgraph", "neo4j"), "4096"));
        // Memgraph into Memgraph: every setting kept.
        assert_eq!(vector(&conv(&from_mg, "memgraph", "memgraph")), mgv);
        // Neptune has no indexes.
        assert!(conv(&cos, "memgraph", "neptune").tables[0]
            .indexes
            .is_empty());
    }

    #[test]
    fn parses_cypher_types() {
        let d = lookup("neo4j").unwrap();
        let p = |s: &str| d.parse_type(&parse(s));
        assert_eq!(p("STRING"), L::Text { unicode: true });
        assert_eq!(p("INTEGER"), L::int(8));
        assert_eq!(p("INTEGER|FLOAT"), L::Float { bytes: 8 });
        assert_eq!(p("BOOLEAN"), L::Bool);
        assert_eq!(p("LIST"), L::Json { binary: true });
        assert_eq!(p("MAP"), L::Json { binary: true });
        assert_eq!(
            p("LIST<INTEGER>"),
            L::Array {
                of: Box::new(L::int(8))
            }
        );
        assert_eq!(
            p("ZONED DATETIME"),
            L::Timestamp {
                precision: Some(9),
                tz: true
            }
        );
        assert!(matches!(p("POINT"), L::Geometry { .. }));
        assert_eq!(p("NULL"), L::Text { unicode: true });
        assert_eq!(p("STRING|INTEGER"), L::Json { binary: true });
    }

    #[test]
    fn renders_cypher_variants() {
        let d = lookup("neo4j").unwrap();
        let r = |t: L| d.render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(4)), "INTEGER");
        assert_eq!(
            r(L::Int {
                bytes: 8,
                unsigned: true
            }),
            "FLOAT"
        );
        assert_eq!(
            r(L::Decimal {
                precision: Some(10),
                scale: Some(2)
            }),
            "FLOAT"
        );
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Money), "FLOAT");
        assert_eq!(
            r(L::Varchar {
                len: Some(2),
                unicode: true
            }),
            "STRING"
        );
        assert_eq!(r(L::Blob), "LIST<INTEGER>");
        assert_eq!(r(L::Bit { len: None }), "STRING");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(
            r(L::Time {
                precision: None,
                tz: true
            }),
            "ZONED TIME"
        );
        assert_eq!(
            r(L::Timestamp {
                precision: None,
                tz: false
            }),
            "LOCAL DATETIME"
        );
        assert_eq!(r(L::Interval), "DURATION");
        assert_eq!(r(L::Year), "INTEGER");
        assert_eq!(r(L::Uuid), "STRING");
        assert_eq!(r(L::Json { binary: true }), "STRING");
        assert_eq!(r(L::Xml), "STRING");
        assert_eq!(r(L::Enum { values: vec![] }), "STRING");
        assert_eq!(r(L::Set { values: vec![] }), "LIST<STRING>");
        assert_eq!(
            r(L::Array {
                of: Box::new(L::int(4))
            }),
            "LIST<INTEGER>"
        );
        assert_eq!(
            r(L::Array {
                of: Box::new(L::Array {
                    of: Box::new(L::int(4))
                })
            }),
            "STRING"
        );
        assert_eq!(
            r(L::Map {
                key: Box::new(L::Bool),
                value: Box::new(L::Bool)
            }),
            "STRING"
        );
        assert_eq!(
            r(L::Geometry {
                kind: Some("point".into()),
                srid: None,
                geography: false
            }),
            "POINT"
        );
        assert_eq!(
            r(L::Geometry {
                kind: None,
                srid: None,
                geography: false
            }),
            "STRING"
        );
        assert_eq!(r(L::Inet), "STRING");
        assert_eq!(r(L::MacAddr), "STRING");
        assert_eq!(r(L::RowVersion), "STRING");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
        assert!(!lookup("neptune").unwrap().caps().indexes);
    }

    #[test]
    fn orientdb_types() {
        let d = lookup("orientdb").unwrap();
        let p = |s: &str| d.parse_type(&parse(s));
        assert_eq!(p("STRING"), L::Text { unicode: true });
        assert_eq!(p("LONG"), L::int(8));
        assert_eq!(p("LONG|DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(
            p("DATETIME"),
            L::Timestamp {
                precision: Some(3),
                tz: true
            }
        );
        assert_eq!(p("EMBEDDEDLIST"), L::Json { binary: true });
        assert_eq!(
            p("LINK"),
            L::Varchar {
                len: Some(32),
                unicode: false
            }
        );
        let r = |t: L| d.render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(1)), "BYTE");
        assert_eq!(r(L::int(2)), "SHORT");
        assert_eq!(r(L::int(4)), "INTEGER");
        assert_eq!(r(L::int(8)), "LONG");
        assert_eq!(
            r(L::Int {
                bytes: 8,
                unsigned: true
            }),
            "DECIMAL"
        );
        assert_eq!(
            r(L::Decimal {
                precision: None,
                scale: None
            }),
            "DECIMAL"
        );
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Float { bytes: 8 }), "DOUBLE");
        assert_eq!(r(L::Money), "DECIMAL");
        assert_eq!(r(L::Text { unicode: true }), "STRING");
        assert_eq!(r(L::Blob), "BINARY");
        assert_eq!(r(L::Bit { len: None }), "STRING");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(
            r(L::Time {
                precision: None,
                tz: false
            }),
            "STRING"
        );
        assert_eq!(
            r(L::Timestamp {
                precision: None,
                tz: true
            }),
            "DATETIME"
        );
        assert_eq!(r(L::Interval), "STRING");
        assert_eq!(r(L::Year), "SHORT");
        assert_eq!(r(L::Uuid), "STRING");
        assert_eq!(r(L::Json { binary: true }), "EMBEDDED");
        assert_eq!(r(L::Xml), "STRING");
        assert_eq!(r(L::Enum { values: vec![] }), "STRING");
        assert_eq!(r(L::Set { values: vec![] }), "EMBEDDEDSET");
        assert_eq!(
            r(L::Array {
                of: Box::new(L::Bool)
            }),
            "EMBEDDEDLIST"
        );
        assert_eq!(
            r(L::Map {
                key: Box::new(L::Bool),
                value: Box::new(L::Bool)
            }),
            "EMBEDDEDMAP"
        );
        assert_eq!(
            r(L::Geometry {
                kind: None,
                srid: None,
                geography: false
            }),
            "STRING"
        );
        assert_eq!(r(L::Inet), "STRING");
        assert_eq!(r(L::MacAddr), "STRING");
        assert_eq!(r(L::RowVersion), "BINARY");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
        assert_eq!(
            d.render_default(
                &DefaultValue::CurrentTimestamp,
                &L::Timestamp {
                    precision: None,
                    tz: true
                }
            )
            .as_deref(),
            Some("sysdate()")
        );
    }
}
