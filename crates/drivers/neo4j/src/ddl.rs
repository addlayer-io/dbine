//! The index / constraint designer, Cypher DDL, create templates and
//! insert scripts, per engine flavor.
//!
//! The designer edits a [`TableSchema`] of kind `index`: `name` is the
//! index (or constraint) name, the columns are the properties, and the
//! table options say what it applies to (`target`: a label or a
//! relationship type; `entity`: `node` / `relationship`) and what it is
//! (`index_type`: an index kind or a constraint kind).
//!
//! Labels ([`crate::LABEL`]) in a database script have no CREATE of their
//! own in Cypher (they exist while a node has them): their part of the
//! script is their indexes and constraints.

use crate::cypher::{
    comment_text, ident, property, property_typed, string, versioned, writes_vector,
};
use crate::{Flavor, LABEL, RELATIONSHIP};
use dbine_driver::vector::{metric, param};
use dbine_driver::{
    kinds, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, IndexDef, ObjectRef,
    Result, ResultColumn, RowChange, TableSchema, VectorIndex, VectorIndexSpec, VectorParamSpec,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub const CONSTRAINT: &str = "constraint";

/// Index kinds and constraint kinds the designer offers.
fn index_types(f: Flavor) -> Vec<(&'static str, &'static str)> {
    match f {
        Flavor::Neo4j => vec![
            ("RANGE", "Índice de rango"),
            ("TEXT", "Índice de texto"),
            ("POINT", "Índice espacial (point)"),
            ("FULLTEXT", "Índice de texto completo"),
            ("VECTOR", "Índice vectorial"),
            ("UNIQUE", "Restricción de unicidad"),
            ("EXISTS", "Restricción de existencia (NOT NULL)"),
            ("KEY", "Restricción de clave"),
        ],
        Flavor::Memgraph => vec![
            ("RANGE", "Índice por etiqueta/propiedad"),
            ("TEXT", "Índice de texto"),
            ("POINT", "Índice espacial (point)"),
            ("VECTOR", "Índice vectorial"),
            ("UNIQUE", "Restricción de unicidad"),
            ("EXISTS", "Restricción de existencia"),
        ],
        Flavor::Neptune => Vec::new(),
    }
}

/// The vector index settings the designer asks for, shown with the
/// VECTOR type and stored under the engine's own names (Neo4j's
/// `indexConfig`, Memgraph's `WITH CONFIG`).
fn vector_fields(f: Flavor) -> Vec<Field> {
    let vector = &["VECTOR"];
    match f {
        Flavor::Neo4j => vec![
            Field::new("vector.dimensions", "Dimensiones", FieldKind::Number)
                .placeholder("1536")
                .when("index_type", vector),
            Field::new(
                "vector.similarity_function",
                "Similitud",
                FieldKind::Select(vec![("COSINE", "Coseno"), ("EUCLIDEAN", "Euclídea")]),
            )
            .default_value("COSINE")
            .when("index_type", vector),
            Field::new(
                "vector.hnsw.m",
                "Conexiones por nodo (M)",
                FieldKind::Number,
            )
            .placeholder("16")
            .when("index_type", vector),
            Field::new(
                "vector.hnsw.ef_construction",
                "Candidatos al construir (ef_construction)",
                FieldKind::Number,
            )
            .placeholder("100")
            .when("index_type", vector),
            Field::new(
                "vector.quantization.enabled",
                "Cuantización",
                FieldKind::Select(vec![("true", "Activada"), ("false", "Desactivada")]),
            )
            .help("Guarda los vectores del índice comprimidos: ocupa menos y pierde algo de precisión (Neo4j 5.23 o posterior).")
            .when("index_type", vector),
        ],
        Flavor::Memgraph => vec![
            Field::new("dimension", "Dimensiones", FieldKind::Number)
                .placeholder("1536")
                .when("index_type", vector),
            Field::new(
                "metric",
                "Métrica",
                FieldKind::Select(vec![
                    ("cos", "Coseno"),
                    ("l2sq", "Euclídea al cuadrado (l2sq)"),
                    ("ip", "Producto escalar"),
                    ("hamming", "Hamming"),
                    ("tanimoto", "Tanimoto"),
                    ("sorensen", "Sorensen"),
                    ("pearson", "Pearson"),
                    ("haversine", "Haversine"),
                    ("divergence", "Divergencia"),
                ]),
            )
            .default_value("cos")
            .when("index_type", vector),
            Field::new("capacity", "Capacidad", FieldKind::Number)
                .placeholder("1000")
                .when("index_type", vector),
            Field::new(
                "scalar_kind",
                "Tipo de los números",
                FieldKind::Select(vec![
                    ("f32", "f32"),
                    ("f16", "f16"),
                    ("bf16", "bf16"),
                    ("f64", "f64"),
                    ("i8", "i8"),
                    ("b1x8", "b1x8"),
                ]),
            )
            .default_value("f32")
            .when("index_type", vector),
            Field::new(
                "resize_coefficient",
                "Coeficiente de crecimiento",
                FieldKind::Number,
            )
            .placeholder("2")
            .help("Cuánto crece la capacidad del índice cuando se llena.")
            .when("index_type", vector),
        ],
        Flavor::Neptune => Vec::new(),
    }
}

/// Without the quotes a catalog may leave around a setting.
fn bare(v: &str) -> &str {
    v.trim().trim_matches(|c| c == '\'' || c == '"')
}

/// A vector index's settings, checked before anything runs (the server
/// says the same, but only once the script started): Neo4j's limits
/// (dimensions 1–4096, M 1–512, ef_construction 1–3200, similarity cosine
/// or euclidean, quantization on or off) and Memgraph's (whole numbers
/// above zero, its metrics and number types).
pub fn check_vector(f: Flavor, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
    let int =
        |v: &str, lo: i64, hi: i64| bare(v).parse::<i64>().is_ok_and(|n| (lo..=hi).contains(&n));
    for (k, v) in o {
        let v = v.as_str();
        let why = match (f, k.as_str()) {
            (Flavor::Neo4j, "vector.dimensions") if !int(v, 1, 4096) => "va de 1 a 4096",
            (Flavor::Neo4j, "vector.hnsw.m") if !int(v, 1, 512) => "va de 1 a 512",
            (Flavor::Neo4j, "vector.hnsw.ef_construction") if !int(v, 1, 3200) => "va de 1 a 3200",
            (Flavor::Neo4j, "vector.similarity_function")
                if !matches!(
                    bare(v).to_ascii_uppercase().as_str(),
                    "COSINE" | "EUCLIDEAN"
                ) =>
            {
                "es COSINE o EUCLIDEAN"
            }
            (Flavor::Neo4j, "vector.quantization.enabled")
                if !matches!(bare(v).to_ascii_lowercase().as_str(), "true" | "false") =>
            {
                "es true o false"
            }
            (Flavor::Memgraph, "dimension" | "capacity" | "resize_coefficient")
                if !int(v, 1, i64::MAX) =>
            {
                "es un número entero mayor que cero"
            }
            (Flavor::Memgraph, "metric")
                if !matches!(
                    bare(v),
                    "cos"
                        | "l2sq"
                        | "ip"
                        | "hamming"
                        | "tanimoto"
                        | "sorensen"
                        | "pearson"
                        | "haversine"
                        | "divergence"
                ) =>
            {
                "no es una métrica de Memgraph"
            }
            (Flavor::Memgraph, "scalar_kind")
                if !matches!(bare(v), "f32" | "f16" | "bf16" | "f64" | "i8" | "b1x8") =>
            {
                "no es un tipo de número de Memgraph"
            }
            _ => continue,
        };
        return Err(Error::Query(format!(
            "Índice vectorial {name}: {k} = {} no sirve ({why}).",
            v.trim()
        )));
    }
    Ok(())
}

/// What a vector index takes ([`DesignerSpec::vector_indexes`]): Neo4j's
/// HNSW with cosine or euclidean similarity; Memgraph's (usearch) with its
/// metrics. The dimension goes in the index on both.
pub fn vector_index_spec(f: Flavor) -> Option<VectorIndexSpec> {
    let p = |key, default| VectorParamSpec {
        key,
        algorithms: Vec::new(),
        default,
    };
    match f {
        Flavor::Neo4j => Some(VectorIndexSpec {
            algorithms: Vec::new(),
            metrics: vec![metric::COSINE, metric::L2],
            params: vec![
                p(DIMENSIONS, ""),
                p(param::M, "16"),
                p(param::EF_CONSTRUCTION, "100"),
                p(param::QUANTIZATION, ""),
            ],
            filter_columns: false,
        }),
        Flavor::Memgraph => Some(VectorIndexSpec {
            algorithms: Vec::new(),
            metrics: vec![
                metric::COSINE,
                metric::L2,
                metric::IP,
                metric::HAMMING,
                "tanimoto",
                "sorensen",
                "pearson",
                "haversine",
                "divergence",
            ],
            params: vec![
                p(DIMENSIONS, ""),
                p("capacity", "1000"),
                p(param::QUANTIZATION, ""),
                p("resize_coefficient", ""),
            ],
            filter_columns: false,
        }),
        Flavor::Neptune => None,
    }
}

/// The vector parameter with the dimension (both engines keep it in the
/// index).
pub const DIMENSIONS: &str = "dimensions";

/// A vector index's settings among Neo4j's `indexConfig` entries.
const NEO4J_VECTOR: &[&str] = &[
    "vector.dimensions",
    "vector.similarity_function",
    "vector.hnsw.m",
    "vector.hnsw.ef_construction",
    "vector.quantization.enabled",
];
/// …and among Memgraph's `WITH CONFIG` entries.
const MEMGRAPH_VECTOR: &[&str] = &[
    "dimension",
    "metric",
    "capacity",
    "scalar_kind",
    "resize_coefficient",
];

fn vector_keys(f: Flavor) -> &'static [&'static str] {
    match f {
        Flavor::Neo4j => NEO4J_VECTOR,
        Flavor::Memgraph => MEMGRAPH_VECTOR,
        Flavor::Neptune => &[],
    }
}

/// A vector index's settings from its options (as the catalog reads them,
/// defaults left out), in the standard names where they fit.
pub fn vector_of(f: Flavor, o: &BTreeMap<String, String>) -> VectorIndex {
    let mut v = VectorIndex::default();
    let get = |k: &str| o.get(k).map(|x| x.trim().to_string());
    match f {
        Flavor::Neo4j => {
            v.algorithm = Some("hnsw".into());
            v.metric =
                get("vector.similarity_function").map(|m| match m.to_ascii_uppercase().as_str() {
                    "COSINE" => metric::COSINE.to_string(),
                    "EUCLIDEAN" => metric::L2.to_string(),
                    other => other.to_ascii_lowercase(),
                });
            for (k, std) in [
                ("vector.dimensions", DIMENSIONS),
                ("vector.hnsw.m", param::M),
                ("vector.hnsw.ef_construction", param::EF_CONSTRUCTION),
            ] {
                if let Some(x) = get(k) {
                    v.params.insert(std.to_string(), x);
                }
            }
            // Only `false` is shown (`true` is the default).
            if get("vector.quantization.enabled").is_some_and(|q| q.eq_ignore_ascii_case("false")) {
                v.params.insert(param::QUANTIZATION.into(), "none".into());
            }
        }
        Flavor::Memgraph => {
            v.metric = get("metric").map(|m| match m.as_str() {
                "cos" => metric::COSINE.to_string(),
                "l2sq" => metric::L2.to_string(),
                other => other.to_string(),
            });
            if let Some(d) = get("dimension") {
                v.params.insert(DIMENSIONS.into(), d);
            }
            if let Some(c) = get("capacity") {
                v.params.insert("capacity".into(), c);
            }
            if let Some(k) = get("scalar_kind").filter(|k| k != "f32") {
                v.params.insert(param::QUANTIZATION.into(), k);
            }
            if let Some(r) = get("resize_coefficient") {
                v.params.insert("resize_coefficient".into(), r);
            }
        }
        Flavor::Neptune => {}
    }
    v
}

/// A vector index's settings as the engine's options: what
/// [`IndexSpec::options`] holds for it.
pub fn vector_options(f: Flavor, v: &VectorIndex) -> BTreeMap<String, String> {
    let mut o = BTreeMap::new();
    let p = |k: &str| {
        v.params
            .get(k)
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
    };
    let off = |q: &str| {
        matches!(
            q.to_ascii_lowercase().as_str(),
            "none" | "false" | "off" | "f32"
        )
    };
    match f {
        Flavor::Neo4j => {
            if let Some(m) = &v.metric {
                let m = match m.as_str() {
                    metric::COSINE => "COSINE".to_string(),
                    metric::L2 => "EUCLIDEAN".into(),
                    other => other.to_ascii_uppercase(),
                };
                o.insert("vector.similarity_function".into(), m);
            }
            for (std, k) in [
                (DIMENSIONS, "vector.dimensions"),
                (param::M, "vector.hnsw.m"),
                (param::EF_CONSTRUCTION, "vector.hnsw.ef_construction"),
            ] {
                if let Some(x) = p(std) {
                    o.insert(k.into(), x);
                }
            }
            if p(param::QUANTIZATION).is_some_and(|q| off(&q)) {
                o.insert("vector.quantization.enabled".into(), "false".into());
            }
        }
        Flavor::Memgraph => {
            if let Some(m) = &v.metric {
                let m = match m.as_str() {
                    metric::COSINE => "cos",
                    metric::L2 => "l2sq",
                    other => other,
                };
                o.insert("metric".into(), m.into());
            }
            if let Some(d) = p(DIMENSIONS) {
                o.insert("dimension".into(), d);
            }
            if let Some(c) = p("capacity") {
                o.insert("capacity".into(), c);
            }
            if let Some(r) = p("resize_coefficient") {
                o.insert("resize_coefficient".into(), r);
            }
            if let Some(q) = p(param::QUANTIZATION).filter(|q| !off(q)) {
                let k = match q.to_ascii_lowercase().as_str() {
                    "int8" | "i8" => "i8".to_string(),
                    "half" | "halfvec" | "float16" | "f16" => "f16".into(),
                    "bfloat16" | "bf16" => "bf16".into(),
                    "binary" | "bit" | "b1" | "b1x8" => "b1x8".into(),
                    _ => q,
                };
                o.insert("scalar_kind".into(), k);
            }
        }
        Flavor::Neptune => {}
    }
    o
}

pub fn designer(f: Flavor) -> Option<DesignerSpec> {
    if f == Flavor::Neptune {
        // Neptune indexes everything by itself and has no constraints.
        return None;
    }
    let mut table_options = vec![
        Field::new("target", "Etiqueta o tipo de relación", FieldKind::Text).required().placeholder("Persona"),
        Field::new("entity", "Se aplica a", FieldKind::Select(vec![("node", "Nodos (etiqueta)"), ("relationship", "Relaciones (tipo)")]))
            .default_value("node"),
        Field::new("index_type", "Tipo", FieldKind::Select(index_types(f)))
            .default_value("RANGE")
            .help("Los índices aceleran búsquedas; las restricciones además validan los datos (y crean su índice)."),
    ];
    table_options.extend(vector_fields(f));
    Some(DesignerSpec {
        kind: kinds::INDEX,
        label: "Nuevo índice",
        data_types: Vec::new(),
        schemas: false,
        primary_key: false,
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: false,
        indexes: false,
        foreign_keys: false,
        column_options: Vec::new(),
        table_options,
        // The columns are the properties; a Memgraph label index needs none.
        columns_required: f == Flavor::Neo4j,
        vector_indexes: vector_index_spec(f),
    })
}

pub fn templates(f: Flavor) -> Vec<CreateTemplate> {
    let t = |kind, label, template: &str| CreateTemplate {
        kind,
        label,
        template: template.to_string(),
    };
    match f {
        Flavor::Neo4j => vec![
            t(LABEL, "Nuevo nodo", "CREATE (n:{name} {nombre: 'valor'})\nRETURN n"),
            t(RELATIONSHIP, "Nueva relación", "MATCH (a:Origen {id: 1}), (b:Destino {id: 2})\nCREATE (a)-[r:{name} {desde: date()}]->(b)\nRETURN r"),
            t(kinds::INDEX, "Nuevo índice", "CREATE INDEX {name} IF NOT EXISTS\nFOR (n:Etiqueta) ON (n.propiedad)"),
            t(CONSTRAINT, "Nueva restricción", "CREATE CONSTRAINT {name} IF NOT EXISTS\nFOR (n:Etiqueta) REQUIRE n.propiedad IS UNIQUE"),
            t(kinds::INDEX, "Nuevo índice de texto completo", "CREATE FULLTEXT INDEX {name} IF NOT EXISTS\nFOR (n:Etiqueta) ON EACH [n.titulo, n.texto]"),
            t(kinds::INDEX, "Nuevo índice vectorial", "CREATE VECTOR INDEX {name} IF NOT EXISTS\nFOR (n:Etiqueta) ON (n.embedding)\nOPTIONS {indexConfig: {`vector.dimensions`: 1536, `vector.similarity_function`: 'cosine'}}"),
        ],
        Flavor::Memgraph => vec![
            t(LABEL, "Nuevo nodo", "CREATE (n:{name} {nombre: 'valor'})\nRETURN n"),
            t(RELATIONSHIP, "Nueva relación", "MATCH (a:Origen {id: 1}), (b:Destino {id: 2})\nCREATE (a)-[r:{name} {desde: date()}]->(b)\nRETURN r"),
            t(kinds::INDEX, "Nuevo índice", "CREATE INDEX ON :Etiqueta(propiedad)"),
            t(CONSTRAINT, "Nueva restricción", "CREATE CONSTRAINT ON (n:Etiqueta) ASSERT n.propiedad IS UNIQUE"),
            t(kinds::TRIGGER, "Nuevo trigger", "CREATE TRIGGER {name}\nON () CREATE AFTER COMMIT EXECUTE\nUNWIND createdVertices AS v\nSET v.creado = timestamp()"),
        ],
        Flavor::Neptune => vec![
            t(LABEL, "Nuevo nodo", "CREATE (n:{name} {nombre: 'valor'})\nRETURN n"),
            t(RELATIONSHIP, "Nueva relación", "MATCH (a:Origen {id: 1}), (b:Destino {id: 2})\nCREATE (a)-[r:{name}]->(b)\nRETURN r"),
        ],
    }
}

/// One index / constraint, as the designer describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSpec {
    pub name: String,
    pub target: String,
    pub relationship: bool,
    /// RANGE, TEXT, POINT, FULLTEXT, VECTOR, LOOKUP, UNIQUE, EXISTS, KEY,
    /// TYPE (a property type constraint).
    pub kind: String,
    pub properties: Vec<String>,
    /// The index's `indexConfig` entries (`fulltext.analyzer`,
    /// `vector.dimensions`…) that aren't the default, and a TYPE
    /// constraint's `propertyType`. Text values go unquoted, the rest as
    /// JSON.
    pub options: BTreeMap<String, String>,
}

/// The `capacity` a Memgraph vector index is made with when it doesn't say.
const MEMGRAPH_VECTOR_CAPACITY: u32 = 1000;

/// The option of a property type constraint.
pub const PROPERTY_TYPE: &str = "propertyType";
/// The option of a full-text index over several labels (or relationship
/// types): all of them, comma-separated.
pub const TARGETS: &str = "labelsOrTypes";

/// A Cypher literal for an option value: numbers, booleans and lists as
/// written, anything else as a string.
fn option_literal(v: &str) -> String {
    match serde_json::from_str::<Value>(v.trim()) {
        Ok(j @ (Value::Number(_) | Value::Bool(_) | Value::Array(_))) => j.to_string(),
        _ => string(v),
    }
}

/// `OPTIONS {indexConfig: {…}}` for the index's settings ("" when none).
fn index_options(s: &IndexSpec) -> String {
    // Only Neo4j's own settings (another engine's options don't carry over).
    let own = |k: &str| {
        ["fulltext.", "vector.", "spatial."]
            .iter()
            .any(|p| k.starts_with(p))
    };
    let cfg: Vec<String> = s
        .options
        .iter()
        .filter(|(k, _)| own(k))
        .map(|(k, v)| format!("{}: {}", crate::cypher::quoted(k), option_literal(v)))
        .collect();
    if cfg.is_empty() {
        String::new()
    } else {
        format!("\nOPTIONS {{indexConfig: {{{}}}}}", cfg.join(", "))
    }
}

impl IndexSpec {
    pub fn is_constraint(&self) -> bool {
        matches!(self.kind.as_str(), "UNIQUE" | "EXISTS" | "KEY" | "TYPE")
    }

    /// It goes into the statement as is, so only a type's characters pass.
    fn property_type(&self) -> Result<&str> {
        let t = self
            .options
            .get(PROPERTY_TYPE)
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                Error::Query(format!(
                    "La restricción de tipo {} no dice el tipo ({PROPERTY_TYPE}).",
                    self.name
                ))
            })?;
        let ok = |c: char| c.is_ascii_alphanumeric() || " _<>|(),".contains(c);
        if !t.chars().all(ok) {
            return Err(Error::Query(format!(
                "{PROPERTY_TYPE}: «{t}» no es un valor válido"
            )));
        }
        Ok(t)
    }

    fn from_designer(f: Flavor, t: &TableSchema) -> Result<Self> {
        let target = t
            .options
            .get("target")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(target) = target else {
            return Err(Error::Query(
                "Indicá la etiqueta o el tipo de relación.".into(),
            ));
        };
        let kind = t
            .options
            .get("index_type")
            .map(|s| s.to_ascii_uppercase())
            .unwrap_or_else(|| "RANGE".into());
        // A vector index's settings (`vector_fields`): the ones given, and
        // not Memgraph's default element type (the catalog doesn't show it).
        let options = if kind == "VECTOR" {
            t.options
                .iter()
                .filter(|(k, v)| {
                    vector_keys(f).contains(&k.as_str())
                        && !v.trim().is_empty()
                        && !(k.as_str() == "scalar_kind" && v.trim() == "f32")
                })
                .map(|(k, v)| (k.clone(), v.trim().to_string()))
                .collect()
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            name: t.name.trim().to_string(),
            target,
            relationship: t.options.get("entity").map(String::as_str) == Some("relationship"),
            kind,
            properties: t
                .columns
                .iter()
                .map(|c| c.name.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect(),
            options,
        })
    }

    /// From an [`IndexDef`] of a label's schema (`kind` holds the type). A
    /// vector index's settings ([`IndexDef::vector`]) win over its options,
    /// in `f`'s names.
    pub fn from_index(f: Flavor, target: &str, relationship: bool, ix: &IndexDef) -> Self {
        let kind = match &ix.vector {
            Some(_) => "VECTOR".to_string(),
            None => ix.kind.clone().unwrap_or_else(|| {
                if ix.unique {
                    "UNIQUE".into()
                } else {
                    "RANGE".into()
                }
            }),
        };
        let options = match &ix.vector {
            Some(v) => {
                // Either engine's settings (Neo4j and Memgraph are one
                // family: an index may come from the other).
                let vector_key = |k: &str| {
                    k.starts_with("vector.")
                        || NEO4J_VECTOR.contains(&k)
                        || MEMGRAPH_VECTOR.contains(&k)
                };
                let mut o: BTreeMap<String, String> = ix
                    .options
                    .iter()
                    .filter(|(k, _)| !vector_key(k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                o.extend(vector_options(f, v));
                o
            }
            None => ix.options.clone(),
        };
        let target = ix
            .options
            .get(TARGETS)
            .map(String::as_str)
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(target);
        Self {
            name: ix.name.clone(),
            target: target.to_string(),
            relationship,
            kind,
            properties: ix.columns.clone(),
            options,
        }
    }
}

/// `CREATE …` for an index or constraint.
pub fn create(f: Flavor, s: &IndexSpec, if_not_exists: bool) -> Result<String> {
    if f == Flavor::Neptune {
        return Err(Error::Unsupported(
            "Neptune no tiene índices ni restricciones definidos por el usuario".into(),
        ));
    }
    let props = &s.properties;
    let needs_props = !(f == Flavor::Memgraph && matches!(s.kind.as_str(), "RANGE" | "TEXT"))
        && s.kind != "LOOKUP";
    if needs_props && props.is_empty() {
        return Err(Error::Query("Agregá al menos una propiedad.".into()));
    }
    let t = ident(&s.target);
    Ok(match f {
        Flavor::Neo4j => {
            let pattern = if s.relationship {
                format!("()-[e:{t}]-()")
            } else {
                format!("(e:{t})")
            };
            let name = if s.name.is_empty() {
                String::new()
            } else {
                format!(" {}", ident(&s.name))
            };
            let ine = if if_not_exists { " IF NOT EXISTS" } else { "" };
            let on = |p: &[String]| {
                p.iter()
                    .map(|p| format!("e.{}", ident(p)))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let tuple = |p: &[String]| {
                if p.len() == 1 {
                    on(p)
                } else {
                    format!("({})", on(p))
                }
            };
            match s.kind.as_str() {
                "UNIQUE" => format!("CREATE CONSTRAINT{name}{ine}\nFOR {pattern} REQUIRE {} IS UNIQUE", tuple(props)),
                "KEY" => {
                    let k = if s.relationship { "RELATIONSHIP KEY" } else { "NODE KEY" };
                    format!("CREATE CONSTRAINT{name}{ine}\nFOR {pattern} REQUIRE {} IS {k}", tuple(props))
                }
                "EXISTS" => {
                    if props.len() != 1 {
                        return Err(Error::Query("Una restricción de existencia lleva una sola propiedad.".into()));
                    }
                    format!("CREATE CONSTRAINT{name}{ine}\nFOR {pattern} REQUIRE {} IS NOT NULL", on(props))
                }
                "TYPE" => {
                    if props.len() != 1 {
                        return Err(Error::Query("Una restricción de tipo lleva una sola propiedad.".into()));
                    }
                    format!("CREATE CONSTRAINT{name}{ine}\nFOR {pattern} REQUIRE {} IS :: {}", on(props), s.property_type()?)
                }
                "FULLTEXT" => {
                    // Several labels (or types) are `A|B`.
                    let targets = s.target.split(',').map(|x| ident(x.trim())).collect::<Vec<_>>().join("|");
                    let pattern = if s.relationship { format!("()-[e:{targets}]-()") } else { format!("(e:{targets})") };
                    format!("CREATE FULLTEXT INDEX{name}{ine}\nFOR {pattern} ON EACH [{}]{}", on(props), index_options(s))
                }
                "VECTOR" if !s.options.keys().any(|k| k.starts_with("vector.")) => format!(
                    "CREATE VECTOR INDEX{name}{ine}\nFOR {pattern} ON ({})\nOPTIONS {{indexConfig: {{`vector.dimensions`: 1536, `vector.similarity_function`: 'cosine'}}}}",
                    on(props)
                ),
                "VECTOR" => {
                    check_vector(f, &s.name, &s.options)?;
                    format!("CREATE VECTOR INDEX{name}{ine}\nFOR {pattern} ON ({}){}", on(props), index_options(s))
                }
                "TEXT" | "POINT" => format!("CREATE {} INDEX{name}{ine}\nFOR {pattern} ON ({}){}", s.kind, on(props), index_options(s)),
                "LOOKUP" => {
                    let f = if s.relationship { "()-[e]-() ON EACH type(e)" } else { "(e) ON EACH labels(e)" };
                    format!("CREATE LOOKUP INDEX{name}{ine}\nFOR {f}")
                }
                _ => format!("CREATE INDEX{name}{ine}\nFOR {pattern} ON ({}){}", on(props), index_options(s)),
            }
        }
        Flavor::Memgraph => {
            let one = |what: &str| -> Result<String> {
                match props.as_slice() {
                    [p] => Ok(ident(p)),
                    _ => Err(Error::Query(format!(
                        "En Memgraph, {what} lleva una sola propiedad."
                    ))),
                }
            };
            let edge = if s.relationship { "EDGE " } else { "" };
            match s.kind.as_str() {
                "UNIQUE" => format!(
                    "CREATE CONSTRAINT ON (n:{t}) ASSERT {} IS UNIQUE",
                    props
                        .iter()
                        .map(|p| format!("n.{}", ident(p)))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "EXISTS" => format!(
                    "CREATE CONSTRAINT ON (n:{t}) ASSERT EXISTS (n.{})",
                    one("una restricción de existencia")?
                ),
                "TYPE" => format!(
                    "CREATE CONSTRAINT ON (n:{t}) ASSERT n.{} IS TYPED {}",
                    one("una restricción de tipo")?,
                    s.property_type()?
                ),
                "TEXT" => format!(
                    "CREATE TEXT INDEX {} ON :{t}",
                    ident(if s.name.is_empty() {
                        &s.target
                    } else {
                        &s.name
                    })
                ),
                "VECTOR" => {
                    if s.name.is_empty() {
                        return Err(Error::Query(
                            "Un índice vectorial de Memgraph necesita un nombre.".into(),
                        ));
                    }
                    if !s.options.contains_key("dimension") {
                        return Err(Error::Query(
                            "Un índice vectorial de Memgraph necesita la dimensión de los vectores.".into(),
                        ));
                    }
                    check_vector(f, &s.name, &s.options)?;
                    let mut cfg: Vec<String> = s
                        .options
                        .iter()
                        .map(|(k, v)| {
                            format!("{}: {}", Value::String(k.clone()), option_literal(v))
                        })
                        .collect();
                    // Required; the server grows it as needed.
                    if !s.options.contains_key("capacity") {
                        cfg.push(format!("\"capacity\": {MEMGRAPH_VECTOR_CAPACITY}"));
                    }
                    format!(
                        "CREATE VECTOR {edge}INDEX {} ON :{t}({}) WITH CONFIG {{{}}}",
                        ident(&s.name),
                        one("un índice vectorial")?,
                        cfg.join(", ")
                    )
                }
                "POINT" => format!("CREATE POINT INDEX ON :{t}({})", one("un índice espacial")?),
                _ if props.is_empty() => format!("CREATE {edge}INDEX ON :{t}"),
                _ => format!(
                    "CREATE {edge}INDEX ON :{t}({})",
                    props
                        .iter()
                        .map(|p| ident(p))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
        Flavor::Neptune => unreachable!(),
    })
}

/// `DROP …` for an index or constraint.
pub fn drop(f: Flavor, s: &IndexSpec, if_exists: bool) -> Result<String> {
    Ok(match f {
        Flavor::Neo4j => {
            if s.name.is_empty() {
                return Err(Error::Query(
                    "Para borrar un índice de Neo4j hace falta su nombre.".into(),
                ));
            }
            let what = if s.is_constraint() {
                "CONSTRAINT"
            } else {
                "INDEX"
            };
            format!(
                "DROP {what} {}{}",
                ident(&s.name),
                if if_exists { " IF EXISTS" } else { "" }
            )
        }
        Flavor::Memgraph => {
            let mut c = create(f, s, false)?;
            // Same statement with DROP: `CREATE INDEX ON :L(p)` → `DROP INDEX ON :L(p)`.
            if s.kind == "TEXT" {
                c = format!(
                    "DROP TEXT INDEX {}",
                    ident(if s.name.is_empty() {
                        &s.target
                    } else {
                        &s.name
                    })
                );
            } else if s.kind == "VECTOR" {
                c = format!("DROP VECTOR INDEX {}", ident(&s.name));
            } else {
                c.replace_range(0..6, "DROP");
            }
            c
        }
        Flavor::Neptune => {
            return Err(Error::Unsupported(
                "Neptune no tiene índices ni restricciones definidos por el usuario".into(),
            ))
        }
    })
}

pub fn table_ddl(f: Flavor, t: &TableSchema, parts: DdlParts) -> Result<String> {
    let mut out: Vec<String> = Vec::new();
    if t.kind == kinds::INDEX || t.kind == CONSTRAINT {
        let s = IndexSpec::from_designer(f, t)?;
        if parts.drop {
            out.push(drop(f, &s, parts.if_exists)?);
        }
        if parts.create || parts.indexes {
            out.push(create(f, &s, parts.if_exists)?);
        }
        return Ok(join(out));
    }
    // A label or relationship type: a comment plus its indexes and constraints.
    let rel = t.kind == RELATIONSHIP;
    if parts.create {
        let what = if rel { "Tipo de relación" } else { "Etiqueta" };
        out.push(format!(
            "// {what} {}: en Cypher no se crea por separado, existe mientras haya {} que la usen",
            comment_text(&ident(&t.name)),
            if rel { "relaciones" } else { "nodos" }
        ));
    }
    if parts.indexes && f != Flavor::Neptune {
        for ix in &t.indexes {
            let s = IndexSpec::from_index(f, &t.name, rel, ix);
            if parts.drop {
                out.push(drop(f, &s, parts.if_exists)?);
            }
            out.push(create(f, &s, parts.if_exists)?);
        }
    }
    Ok(join(out))
}

fn join(stmts: Vec<String>) -> String {
    stmts
        .into_iter()
        .map(|s| {
            if s.starts_with("//") {
                s
            } else {
                format!("{s};")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A graph value (`~entityType`) held in a cell, as JSON.
fn entity(cell: &Value) -> Option<serde_json::Map<String, Value>> {
    let v = match cell {
        Value::String(s) if s.starts_with('{') => serde_json::from_str::<Value>(s).ok()?,
        Value::Object(_) => cell.clone(),
        _ => return None,
    };
    let o = v.as_object()?;
    o.contains_key("~entityType").then(|| o.clone())
}

fn prop_map(props: impl Iterator<Item = (String, Value)>) -> String {
    typed_prop_map(props.map(|(k, v)| (k, v, None)))
}

/// [`prop_map`] with each property written for its column's type
/// ([`property_typed`]).
fn typed_prop_map<'a>(
    props: impl Iterator<Item = (String, Value, Option<&'a ResultColumn>)>,
) -> String {
    let body: Vec<String> = props
        .filter(|(_, v, _)| !v.is_null())
        .map(|(k, v, c)| format!("{}: {}", ident(&k), property_typed(&v, c)))
        .collect();
    if body.is_empty() {
        String::new()
    } else {
        format!(" {{{}}}", body.join(", "))
    }
}

/// Matching a node by the id a source database gave it.
fn match_by_id(f: Flavor, var: &str, id: &Value) -> String {
    match (f, id) {
        (Flavor::Neo4j, Value::String(s)) => format!("elementId({var}) = {}", string(s)),
        // Memgraph's element ids are its numeric ids as text.
        (Flavor::Memgraph, Value::String(s)) if s.parse::<i64>().is_ok() => {
            format!("id({var}) = {s}")
        }
        (_, Value::String(s)) => format!("id({var}) = {}", string(s)),
        (_, other) => format!("id({var}) = {other}"),
    }
}

/// `CREATE` per row. A row holding a node (as `MATCH (n:L) RETURN n`
/// shows it) recreates it with its labels and properties; plain columns
/// become the properties of a node with the target's label. For a
/// relationship type, the row's relationship is recreated between the
/// nodes with its original start / end ids (for copies within the same
/// database: other databases give their own ids).
pub fn insert_script(
    f: Flavor,
    target: &ObjectRef,
    columns: &[String],
    rows: &[Vec<Value>],
) -> Result<String> {
    let columns: Vec<ResultColumn> = columns.iter().map(|c| ResultColumn::new(c, "")).collect();
    insert_script_typed(f, target, &columns, rows)
}

/// [`insert_script`] knowing the columns' types: a vector into a Neo4j
/// 2025 `VECTOR<…>(n)` column is written as a `vector(…)`.
pub fn insert_script_typed(
    f: Flavor,
    target: &ObjectRef,
    columns: &[ResultColumn],
    rows: &[Vec<Value>],
) -> Result<String> {
    let mut out = String::new();
    for row in rows {
        let ent = if row.len() == 1 {
            entity(&row[0])
        } else {
            None
        };
        let props_of = |e: &serde_json::Map<String, Value>| {
            // Sorted: serde_json's key order depends on whether another crate
            // in the build enables `preserve_order`.
            e.get("~properties")
                .and_then(Value::as_object)
                .map(|p| {
                    prop_map(
                        p.clone()
                            .into_iter()
                            .collect::<std::collections::BTreeMap<_, _>>()
                            .into_iter(),
                    )
                })
                .unwrap_or_default()
        };
        // A VECTOR value (`vector(…)`) needs Cypher 25.
        let vectors = match &ent {
            Some(e) => e
                .get("~properties")
                .and_then(Value::as_object)
                .is_some_and(|p| p.values().any(|v| writes_vector(v, None))),
            None => columns
                .iter()
                .zip(row)
                .any(|(c, v)| writes_vector(v, Some(c))),
        };
        let stmt = match ent {
            Some(e) if e.get("~entityType").and_then(Value::as_str) == Some("node") => {
                let labels: Vec<String> = e
                    .get("~labels")
                    .and_then(Value::as_array)
                    .map(|l| l.iter().filter_map(Value::as_str).map(ident).collect())
                    .unwrap_or_default();
                let labels = if labels.is_empty() {
                    ident(&target.name)
                } else {
                    labels.join(":")
                };
                format!("CREATE (:{labels}{});", props_of(&e))
            }
            Some(e) if e.get("~entityType").and_then(Value::as_str) == Some("relationship") => {
                let (Some(s), Some(t)) = (e.get("~start"), e.get("~end")) else {
                    return Err(Error::Unsupported(
                        "la relación no trae sus nodos de origen y destino".into(),
                    ));
                };
                let ty = e
                    .get("~type")
                    .and_then(Value::as_str)
                    .unwrap_or(&target.name);
                format!(
                    "MATCH (a), (b) WHERE {} AND {}\nCREATE (a)-[:{}{}]->(b);",
                    match_by_id(f, "a", s),
                    match_by_id(f, "b", t),
                    ident(ty),
                    props_of(&e)
                )
            }
            _ => {
                if target.kind == RELATIONSHIP {
                    return Err(Error::Unsupported(
                        "para copiar relaciones hace falta la relación completa (MATCH ()-[r]->() RETURN r), con sus nodos".into(),
                    ));
                }
                let props = typed_prop_map(
                    columns
                        .iter()
                        .zip(row.iter().cloned())
                        .map(|(c, v)| (c.name.clone(), v, Some(c))),
                );
                format!("CREATE (:{}{props});", ident(&target.name))
            }
        };
        out.push_str(&versioned(stmt, vectors));
        out.push('\n');
    }
    Ok(out)
}

/// `MATCH … SET` per edited row. A row holding a whole node or
/// relationship (as browsing shows it) is matched by its id and its edited
/// value replaces the properties (`SET n = {…}`); plain property columns
/// match on the key properties (`IS NULL` for a null one) and set each
/// edited property (`= null` removes it).
pub fn update_script(f: Flavor, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    update_script_typed(f, target, changes, &[])
}

/// [`update_script`] knowing the columns' types (by name): a vector set
/// into a Neo4j 2025 `VECTOR<…>(n)` property is written as a `vector(…)`.
pub fn update_script_typed(
    f: Flavor,
    target: &ObjectRef,
    changes: &[RowChange],
    types: &[ResultColumn],
) -> Result<String> {
    let rel = target.kind == RELATIONSHIP;
    let var = if rel { "r" } else { "n" };
    let pattern = match target.kind.as_str() {
        RELATIONSHIP => format!("()-[r:{}]->()", ident(&target.name)),
        LABEL => format!("(n:{})", ident(&target.name)),
        _ => "(n)".to_string(),
    };
    let mut out = String::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        let whole = match ch.key.as_slice() {
            [(_, v)] => entity(v),
            _ => None,
        };
        let vectors;
        let stmt = if let Some(e) = whole {
            let id = e
                .get("~id")
                .ok_or_else(|| Error::Unsupported("el elemento no trae su id".into()))?;
            let [(_, new)] = ch.set.as_slice() else {
                return Err(Error::Unsupported(
                    "se edita el elemento completo, como JSON".into(),
                ));
            };
            let props = entity(new)
                .and_then(|n| n.get("~properties").and_then(Value::as_object).cloned())
                .ok_or_else(|| {
                    Error::Unsupported("el valor editado no es un nodo o relación en JSON".into())
                })?;
            vectors = props.values().any(|v| writes_vector(v, None));
            let body: Vec<String> = props
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| format!("{}: {}", ident(&k), property(&v)))
                .collect();
            let (label_pattern, id_var) = if rel {
                ("()-[r]->()".to_string(), "r")
            } else {
                ("(n)".to_string(), "n")
            };
            format!(
                "MATCH {label_pattern} WHERE {}\nSET {id_var} = {{{}}};",
                match_by_id(f, id_var, id),
                body.join(", ")
            )
        } else {
            if ch.key.is_empty() {
                return Err(Error::Unsupported(
                    "no hay propiedades para identificar el elemento".into(),
                ));
            }
            let conds: Vec<String> = ch
                .key
                .iter()
                .map(|(k, v)| {
                    if v.is_null() {
                        format!("{var}.{} IS NULL", ident(k))
                    } else {
                        format!("{var}.{} = {}", ident(k), property(v))
                    }
                })
                .collect();
            let typed = |k: &str| types.iter().find(|c| c.name == k);
            vectors = ch.key.iter().any(|(_, v)| writes_vector(v, None))
                || ch.set.iter().any(|(k, v)| writes_vector(v, typed(k)));
            let sets: Vec<String> = ch
                .set
                .iter()
                .map(|(k, v)| format!("{var}.{} = {}", ident(k), property_typed(v, typed(k))))
                .collect();
            format!(
                "MATCH {pattern} WHERE {}\nSET {};",
                conds.join(" AND "),
                sets.join(", ")
            )
        };
        out.push_str(&versioned(stmt, vectors));
        out.push('\n');
    }
    Ok(out)
}

/// The browse query (`MATCH (n:L) RETURN n LIMIT k`, or `()-[r:T]->()`)
/// restricted by the grid's column filters: a WHERE on the properties
/// (`n.prop`, also written `n.prop` in the column) before RETURN, with
/// Cypher literals and its CONTAINS / STARTS WITH / ENDS WITH. The grid's
/// column holding the whole node or relationship isn't a property: it's
/// left to the grid, as are SQL conditions.
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::FilterOp;
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let var = if browse.trim_start().starts_with("MATCH ()-[r") {
        "r"
    } else {
        "n"
    };
    let at = browse.rfind(" RETURN ").ok_or_else(|| {
        Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into())
    })?;
    let mut parts = Vec::new();
    for f in filters {
        let prop = f
            .column
            .strip_prefix(&format!("{var}."))
            .unwrap_or(&f.column);
        if prop == var {
            return Err(Error::Unsupported(
                "la columna tiene el elemento completo: se filtra por sus propiedades".into(),
            ));
        }
        let c = format!("{var}.{}", ident(prop));
        let first = || {
            f.values.first().ok_or_else(|| {
                Error::Query(format!("el filtro de «{}» necesita un valor", f.column))
            })
        };
        let text = || {
            first().map(|v| {
                string(
                    &v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string()),
                )
            })
        };
        let list = || {
            if f.values.is_empty() {
                return Err(Error::Query(format!(
                    "el filtro de «{}» necesita al menos un valor",
                    f.column
                )));
            }
            Ok(f.values.iter().map(property).collect::<Vec<_>>().join(", "))
        };
        parts.push(match f.op {
            FilterOp::Eq => format!("{c} = {}", property(first()?)),
            FilterOp::Ne => format!("{c} <> {}", property(first()?)),
            FilterOp::Gt => format!("{c} > {}", property(first()?)),
            FilterOp::Ge => format!("{c} >= {}", property(first()?)),
            FilterOp::Lt => format!("{c} < {}", property(first()?)),
            FilterOp::Le => format!("{c} <= {}", property(first()?)),
            FilterOp::Contains => format!("{c} CONTAINS {}", text()?),
            FilterOp::NotContains => format!("NOT {c} CONTAINS {}", text()?),
            FilterOp::StartsWith => format!("{c} STARTS WITH {}", text()?),
            FilterOp::EndsWith => format!("{c} ENDS WITH {}", text()?),
            FilterOp::IsNull => format!("{c} IS NULL"),
            FilterOp::NotNull => format!("{c} IS NOT NULL"),
            FilterOp::IsEmpty => format!("{c} = ''"),
            FilterOp::NotEmpty => format!("({c} IS NOT NULL AND {c} <> '')"),
            FilterOp::In => format!("{c} IN [{}]", list()?),
            FilterOp::NotIn => format!("NOT {c} IN [{}]", list()?),
            FilterOp::IsTrue => format!("{c} = true"),
            FilterOp::IsFalse => format!("{c} = false"),
            FilterOp::TrueOrNull => format!("({c} = true OR {c} IS NULL)"),
            FilterOp::FalseOrNull => format!("({c} = false OR {c} IS NULL)"),
            FilterOp::Sql | FilterOp::SqlRight => {
                return Err(Error::Unsupported(
                    "Cypher no toma condiciones SQL: se filtran en la grilla".into(),
                ))
            }
        });
    }
    Ok(format!(
        "{}\nWHERE {}\n{}",
        &browse[..at],
        parts.join("\n  AND "),
        &browse[at + 1..]
    ))
}

/// The browse query (filtered or not: `MATCH … [WHERE …] RETURN n LIMIT k`)
/// sorted by the grid's columns: `ORDER BY n.prop` between RETURN and
/// LIMIT, replacing an earlier sort. The columns are properties (`prop` or
/// `n.prop`); the one holding the whole node or relationship isn't
/// sortable on the server and is left to the grid.
pub fn sorted_browse(query: &str, sort: &[dbine_driver::SortKey]) -> Result<String> {
    if sort.is_empty() {
        return Ok(query.to_string());
    }
    let cant =
        || Error::Unsupported("no se pudo agregar el orden a la consulta de este objeto".into());
    // RETURN and LIMIT are the browse query's own, so the last ones: a
    // filter value or a label in backticks comes before them.
    let at = query
        .rfind("RETURN ")
        .filter(|&i| i == 0 || query[..i].ends_with(char::is_whitespace))
        .ok_or_else(cant)?;
    let rest = &query[at + "RETURN ".len()..];
    let var: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if var.is_empty() {
        return Err(cant());
    }
    let limit = rest
        .rfind("LIMIT ")
        .map(|i| rest[i..].trim_end())
        .unwrap_or("");
    let mut keys = Vec::new();
    for k in sort {
        let prop = k
            .column
            .strip_prefix(&format!("{var}."))
            .unwrap_or(&k.column);
        if prop == var {
            return Err(Error::Unsupported(
                "la columna tiene el elemento completo: se ordena por sus propiedades".into(),
            ));
        }
        keys.push(format!(
            "{var}.{} {}",
            ident(prop),
            if k.descending { "DESC" } else { "ASC" }
        ));
    }
    let head = format!("{}RETURN {var}\nORDER BY {}", &query[..at], keys.join(", "));
    Ok(if limit.is_empty() {
        head
    } else {
        format!("{head}\n{limit}")
    })
}

/// `MATCH … DELETE` per row, identified like [`update_script`] does: a
/// row holding a whole node or relationship by its id, plain property
/// columns by the key properties (`IS NULL` for a null one). Nodes go with
/// `DETACH DELETE` (their relationships can't outlive them); relationships
/// with `DELETE`.
pub fn delete_script(
    f: Flavor,
    target: &ObjectRef,
    keys: &[Vec<(String, Value)>],
) -> Result<String> {
    let rel = target.kind == RELATIONSHIP;
    let pattern = match target.kind.as_str() {
        RELATIONSHIP => format!("()-[r:{}]->()", ident(&target.name)),
        LABEL => format!("(n:{})", ident(&target.name)),
        _ => "(n)".to_string(),
    };
    let mut out = String::new();
    for key in keys {
        let whole = match key.as_slice() {
            [(_, v)] => entity(v),
            _ => None,
        };
        let stmt = if let Some(e) = whole {
            let id = e
                .get("~id")
                .ok_or_else(|| Error::Unsupported("el elemento no trae su id".into()))?;
            let is_rel = match e.get("~entityType").and_then(Value::as_str) {
                Some(t) => t == "relationship",
                None => rel,
            };
            if is_rel {
                format!(
                    "MATCH ()-[r]->() WHERE {}\nDELETE r;",
                    match_by_id(f, "r", id)
                )
            } else {
                format!(
                    "MATCH (n) WHERE {}\nDETACH DELETE n;",
                    match_by_id(f, "n", id)
                )
            }
        } else {
            if key.is_empty() {
                return Err(Error::Unsupported(
                    "no hay propiedades para identificar el elemento".into(),
                ));
            }
            let var = if rel { "r" } else { "n" };
            let conds: Vec<String> = key
                .iter()
                .map(|(k, v)| {
                    if v.is_null() {
                        format!("{var}.{} IS NULL", ident(k))
                    } else {
                        format!("{var}.{} = {}", ident(k), property(v))
                    }
                })
                .collect();
            let del = if rel { "DELETE r" } else { "DETACH DELETE n" };
            versioned(
                format!("MATCH {pattern} WHERE {}\n{del};", conds.join(" AND ")),
                key.iter().any(|(_, v)| writes_vector(v, None)),
            )
        };
        out.push_str(&stmt);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ColumnDef;
    use serde_json::json;

    #[test]
    fn typed_scripts_keep_native_vectors() {
        let t = ObjectRef {
            kind: LABEL.into(),
            schema: None,
            name: "Doc".into(),
        };
        let cols = [
            ResultColumn::new("id", "INTEGER"),
            ResultColumn::new("e", "VECTOR<FLOAT32>(2)"),
            ResultColumn::new("l", "LIST<FLOAT>"),
        ];
        let row = vec![json!(1), json!([0.5, 1]), json!([0.5, 1])];
        assert_eq!(
            insert_script_typed(Flavor::Neo4j, &t, &cols, std::slice::from_ref(&row)).unwrap(),
            "CYPHER 25 CREATE (:Doc {id: 1, e: vector([0.5, 1], 2, FLOAT32), l: [0.5, 1]});\n"
        );
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        assert_eq!(
            insert_script(Flavor::Neo4j, &t, &names, &[row]).unwrap(),
            "CREATE (:Doc {id: 1, e: [0.5, 1], l: [0.5, 1]});\n"
        );
        let change = RowChange {
            key: vec![("id".into(), json!(1))],
            set: vec![("e".into(), json!([2, -1])), ("l".into(), json!([2, -1]))],
            row: Vec::new(),
        };
        assert_eq!(
            update_script_typed(Flavor::Neo4j, &t, std::slice::from_ref(&change), &cols).unwrap(),
            "CYPHER 25 MATCH (n:Doc) WHERE n.id = 1\nSET n.e = vector([2, -1], 2, FLOAT32), n.l = [2, -1];\n"
        );
        assert_eq!(
            update_script(Flavor::Neo4j, &t, &[change]).unwrap(),
            "MATCH (n:Doc) WHERE n.id = 1\nSET n.e = [2, -1], n.l = [2, -1];\n"
        );
        // A whole node edited as JSON with a VECTOR property, too.
        let node = |e: Value| {
            json!({"~entityType": "node", "~id": "4:a:1", "~labels": ["Doc"], "~properties": {"e": e}})
                .to_string()
        };
        let v = json!({"~vector": [1, 2], "~coordinateType": "FLOAT32"});
        let whole = update_script(
            Flavor::Neo4j,
            &t,
            &[RowChange {
                key: vec![("n".into(), json!(node(v.clone())))],
                set: vec![("n".into(), json!(node(v.clone())))],
                row: Vec::new(),
            }],
        )
        .unwrap();
        assert_eq!(
            whole,
            "CYPHER 25 MATCH (n) WHERE elementId(n) = '4:a:1'\nSET n = {e: vector([1, 2], 2, FLOAT32)};\n"
        );
        let ins = insert_script(Flavor::Neo4j, &t, &["n".into()], &[vec![json!(node(v))]]).unwrap();
        assert!(
            ins.starts_with("CYPHER 25 CREATE (:Doc {e: vector("),
            "{ins}"
        );
    }

    #[test]
    fn filtered_browse_adds_where_to_match() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter {
            column: column.into(),
            op,
            values,
            sql: None,
        };
        assert_eq!(
            filtered_browse(
                "MATCH (n:Person) RETURN n LIMIT 200",
                &[
                    f("name", FilterOp::Eq, vec![json!("O'Brien")]),
                    f("n.born", FilterOp::Gt, vec![json!(1970)]),
                    f("full name", FilterOp::StartsWith, vec![json!("Ke")]),
                    f("died", FilterOp::IsNull, vec![]),
                    f("id", FilterOp::In, vec![json!(1), json!("a")]),
                ]
            )
            .unwrap(),
            "MATCH (n:Person)\nWHERE n.name = 'O\\'Brien'\n  AND n.born > 1970\n  AND n.`full name` STARTS WITH 'Ke'\n  AND n.died IS NULL\n  AND n.id IN [1, 'a']\nRETURN n LIMIT 200"
        );
        assert_eq!(
            filtered_browse(
                "MATCH ()-[r:ACTED_IN]->() RETURN r LIMIT 5",
                &[f("roles", FilterOp::NotNull, vec![])]
            )
            .unwrap(),
            "MATCH ()-[r:ACTED_IN]->()\nWHERE r.roles IS NOT NULL\nRETURN r LIMIT 5"
        );
        assert!(matches!(
            filtered_browse(
                "MATCH (n:P) RETURN n LIMIT 5",
                &[f("n", FilterOp::Contains, vec![json!("x")])]
            ),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn sorted_browse_orders_before_limit() {
        use dbine_driver::{ColumnFilter, FilterOp, SortKey};
        let k = |c: &str, d: bool| SortKey {
            column: c.into(),
            descending: d,
        };
        let browse = "MATCH (n:`Per RETURN son`) RETURN n LIMIT 200";
        assert_eq!(sorted_browse(browse, &[]).unwrap(), browse);
        assert_eq!(
            sorted_browse(browse, &[k("name", false)]).unwrap(),
            "MATCH (n:`Per RETURN son`) RETURN n\nORDER BY n.name ASC\nLIMIT 200"
        );
        assert_eq!(
            sorted_browse(browse, &[k("n.born", true), k("full `name`", false)]).unwrap(),
            "MATCH (n:`Per RETURN son`) RETURN n\nORDER BY n.born DESC, n.`full ``name``` ASC\nLIMIT 200"
        );
        // After the filter, whose values may hold RETURN / LIMIT.
        let filtered = filtered_browse(
            browse,
            &[ColumnFilter {
                column: "name".into(),
                op: FilterOp::Eq,
                values: vec![json!("x RETURN y LIMIT 3")],
                sql: None,
            }],
        )
        .unwrap();
        assert_eq!(
            sorted_browse(&filtered, &[k("born", true)]).unwrap(),
            "MATCH (n:`Per RETURN son`)\nWHERE n.name = 'x RETURN y LIMIT 3'\nRETURN n\nORDER BY n.born DESC\nLIMIT 200"
        );
        // A second sort replaces the first.
        let once = sorted_browse(
            "MATCH ()-[r:ACTED_IN]->() RETURN r LIMIT 5",
            &[k("roles", false)],
        )
        .unwrap();
        assert_eq!(
            sorted_browse(&once, &[k("r.year", true)]).unwrap(),
            "MATCH ()-[r:ACTED_IN]->() RETURN r\nORDER BY r.year DESC\nLIMIT 5"
        );
        assert!(matches!(
            sorted_browse(browse, &[k("n", false)]),
            Err(Error::Unsupported(_))
        ));
    }

    fn designed(kind: &str, entity: &str, props: &[&str]) -> TableSchema {
        TableSchema {
            kind: kinds::INDEX.into(),
            name: "ix".into(),
            columns: props
                .iter()
                .map(|p| ColumnDef {
                    name: p.to_string(),
                    ..Default::default()
                })
                .collect(),
            options: [
                ("target", "Person"),
                ("entity", entity),
                ("index_type", kind),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            ..Default::default()
        }
    }

    fn parts() -> DdlParts {
        DdlParts {
            create: true,
            if_exists: true,
            ..Default::default()
        }
    }

    #[test]
    fn vector_indexes_both_ways() {
        let opts = |kv: &[(&str, &str)]| {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        // Neo4j, as SHOW INDEXES reads it (defaults left out).
        let neo = opts(&[
            ("vector.dimensions", "3"),
            ("vector.similarity_function", "COSINE"),
            ("vector.hnsw.m", "32"),
            ("vector.quantization.enabled", "false"),
        ]);
        let v = vector_of(Flavor::Neo4j, &neo);
        assert_eq!(v.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(v.metric.as_deref(), Some("cosine"));
        assert_eq!(v.params.get(DIMENSIONS).map(String::as_str), Some("3"));
        assert_eq!(v.params.get("m").map(String::as_str), Some("32"));
        assert_eq!(
            v.params.get("quantization").map(String::as_str),
            Some("none")
        );
        assert_eq!(vector_options(Flavor::Neo4j, &v), neo);
        // Memgraph, as SHOW VECTOR INDEX INFO reads it.
        let mg = opts(&[
            ("dimension", "3"),
            ("metric", "l2sq"),
            ("scalar_kind", "f16"),
        ]);
        let v = vector_of(Flavor::Memgraph, &mg);
        assert_eq!(v.metric.as_deref(), Some("l2"));
        assert_eq!(
            v.params.get("quantization").map(String::as_str),
            Some("f16")
        );
        assert_eq!(vector_options(Flavor::Memgraph, &v), mg);
        // One engine's index on the other (same family, so as it is): its
        // settings in the other's names.
        let ix = IndexDef {
            name: "v".into(),
            columns: vec!["emb".into()],
            kind: Some("VECTOR".into()),
            options: neo.clone(),
            vector: Some(vector_of(Flavor::Neo4j, &neo)),
            ..Default::default()
        };
        let on_mg = IndexSpec::from_index(Flavor::Memgraph, "Doc", false, &ix);
        assert_eq!(
            create(Flavor::Memgraph, &on_mg, false).unwrap(),
            "CREATE VECTOR INDEX v ON :Doc(emb) WITH CONFIG {\"dimension\": 3, \"metric\": 'cos', \"capacity\": 1000}"
        );
        let on_neo = IndexSpec::from_index(Flavor::Neo4j, "Doc", false, &ix);
        assert_eq!(
            create(Flavor::Neo4j, &on_neo, true).unwrap(),
            "CREATE VECTOR INDEX v IF NOT EXISTS\nFOR (e:Doc) ON (e.emb)\nOPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.hnsw.m`: 32, `vector.quantization.enabled`: false, `vector.similarity_function`: 'COSINE'}}"
        );
        // Another family's (pgvector's HNSW, through the dialect): only its
        // settings, which win over the options.
        let mut pg = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("l2".into()),
            ..Default::default()
        };
        pg.params.insert("ef_construction".into(), "64".into());
        let ix = IndexDef {
            name: "p".into(),
            columns: vec!["emb".into()],
            options: opts(&[("vector.similarity_function", "COSINE")]),
            vector: Some(pg),
            ..Default::default()
        };
        assert_eq!(
            create(Flavor::Neo4j, &IndexSpec::from_index(Flavor::Neo4j, "Doc", false, &ix), false).unwrap(),
            "CREATE VECTOR INDEX p\nFOR (e:Doc) ON (e.emb)\nOPTIONS {indexConfig: {`vector.hnsw.ef_construction`: 64, `vector.similarity_function`: 'EUCLIDEAN'}}"
        );
        // Memgraph can't make one without the dimension.
        let no_dims = IndexSpec::from_index(Flavor::Memgraph, "Doc", false, &ix);
        assert!(
            matches!(create(Flavor::Memgraph, &no_dims, false), Err(Error::Query(m)) if m.contains("dimensión"))
        );
    }

    /// Settings the server would refuse are refused before anything runs.
    #[test]
    fn vector_settings_are_checked() {
        let o = |kv: &[(&str, &str)]| -> BTreeMap<String, String> {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let ok = o(&[
            ("vector.dimensions", "1536"),
            ("vector.similarity_function", "'cosine'"),
            ("vector.hnsw.m", "16"),
            ("vector.hnsw.ef_construction", "100"),
            ("vector.quantization.enabled", "false"),
        ]);
        assert!(check_vector(Flavor::Neo4j, "v", &ok).is_ok());
        for bad in [
            ("vector.dimensions", "4097"),
            ("vector.dimensions", "0"),
            ("vector.hnsw.m", "513"),
            ("vector.hnsw.ef_construction", "3201"),
            ("vector.similarity_function", "DOT"),
            ("vector.quantization.enabled", "maybe"),
        ] {
            let e = check_vector(Flavor::Neo4j, "v", &o(&[bad]))
                .unwrap_err()
                .to_string();
            assert!(e.contains(bad.0) && e.contains("no sirve"), "{bad:?}: {e}");
        }
        let ok = o(&[
            ("dimension", "3"),
            ("capacity", "1000"),
            ("metric", "cos"),
            ("scalar_kind", "f16"),
            ("resize_coefficient", "2"),
        ]);
        assert!(check_vector(Flavor::Memgraph, "v", &ok).is_ok());
        for bad in [
            ("dimension", "-3"),
            ("capacity", "mil"),
            ("resize_coefficient", "0"),
            ("metric", "manhattan"),
            ("scalar_kind", "f8"),
        ] {
            assert!(
                check_vector(Flavor::Memgraph, "v", &o(&[bad])).is_err(),
                "{bad:?}"
            );
        }
        // Through the designer: the statement isn't written.
        let spec = |opts: &[(&str, &str)]| IndexSpec {
            name: "v".into(),
            target: "Doc".into(),
            relationship: false,
            kind: "VECTOR".into(),
            properties: vec!["emb".into()],
            options: o(opts),
        };
        assert!(create(Flavor::Neo4j, &spec(&[("vector.hnsw.m", "0")]), false).is_err());
        assert!(create(
            Flavor::Memgraph,
            &spec(&[("dimension", "3"), ("metric", "cosine")]),
            false
        )
        .is_err());
        // The designer offers Neo4j's quantization and Memgraph's growth.
        let keys = |f| {
            designer(f)
                .unwrap()
                .table_options
                .iter()
                .map(|x| x.key)
                .collect::<Vec<_>>()
        };
        assert!(keys(Flavor::Neo4j).contains(&"vector.quantization.enabled"));
        assert!(keys(Flavor::Memgraph).contains(&"resize_coefficient"));
    }

    #[test]
    fn the_designer_makes_vector_indexes() {
        for f in [Flavor::Neo4j, Flavor::Memgraph] {
            let d = designer(f).unwrap();
            assert!(d.vector_indexes.is_some());
            let kinds = d
                .table_options
                .iter()
                .find(|o| o.key == "index_type")
                .map(|o| format!("{:?}", o.kind))
                .unwrap();
            assert!(kinds.contains("VECTOR"), "{kinds}");
            // The settings only show with the VECTOR type.
            for o in d.table_options.iter().skip(3) {
                assert_eq!(o.when.as_ref().unwrap().values, ["VECTOR"], "{}", o.key);
            }
        }
        let mut t = designed("VECTOR", "node", &["emb"]);
        for (k, v) in [
            ("vector.dimensions", "3"),
            ("vector.similarity_function", "EUCLIDEAN"),
            ("vector.hnsw.m", ""),
            ("dimension", "9"),
        ] {
            t.options.insert(k.into(), v.into());
        }
        assert_eq!(
            table_ddl(Flavor::Neo4j, &t, parts()).unwrap(),
            "CREATE VECTOR INDEX ix IF NOT EXISTS\nFOR (e:Person) ON (e.emb)\nOPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.similarity_function`: 'EUCLIDEAN'}};"
        );
        let mut t = designed("VECTOR", "node", &["emb"]);
        for (k, v) in [
            ("dimension", "3"),
            ("metric", "ip"),
            ("scalar_kind", "f32"),
            ("capacity", "50"),
        ] {
            t.options.insert(k.into(), v.into());
        }
        assert_eq!(
            table_ddl(Flavor::Memgraph, &t, parts()).unwrap(),
            "CREATE VECTOR INDEX ix ON :Person(emb) WITH CONFIG {\"capacity\": 50, \"dimension\": 3, \"metric\": 'ip'};"
        );
        // Vectors in scripts are lists of floats.
        let target = ObjectRef {
            kind: LABEL.into(),
            schema: None,
            name: "Doc".into(),
        };
        let rows = dbine_driver::vector::rows_for_scripts(
            &[
                dbine_driver::ResultColumn::new("id", ""),
                dbine_driver::ResultColumn::new("emb", "LIST<FLOAT>").with_vector(
                    dbine_driver::VectorMeta::dense(Some(2), dbine_driver::VectorElement::F64),
                ),
            ],
            &[vec![serde_json::json!(1), serde_json::json!("[0.5,2]")]],
        );
        assert_eq!(
            insert_script(Flavor::Neo4j, &target, &["id".into(), "emb".into()], &rows).unwrap(),
            "CREATE (:Doc {id: 1, emb: [0.5, 2.0]});\n"
        );
    }

    #[test]
    fn neo4j_ddl() {
        let f = Flavor::Neo4j;
        assert_eq!(
            table_ddl(f, &designed("RANGE", "node", &["name"]), parts()).unwrap(),
            "CREATE INDEX ix IF NOT EXISTS\nFOR (e:Person) ON (e.name);"
        );
        assert_eq!(
            table_ddl(f, &designed("UNIQUE", "node", &["a", "b"]), parts()).unwrap(),
            "CREATE CONSTRAINT ix IF NOT EXISTS\nFOR (e:Person) REQUIRE (e.a, e.b) IS UNIQUE;"
        );
        assert_eq!(
            table_ddl(f, &designed("EXISTS", "relationship", &["since"]), parts()).unwrap(),
            "CREATE CONSTRAINT ix IF NOT EXISTS\nFOR ()-[e:Person]-() REQUIRE e.since IS NOT NULL;"
        );
        assert!(table_ddl(f, &designed("RANGE", "node", &[]), parts()).is_err());
        let d = DdlParts {
            drop: true,
            if_exists: true,
            create: true,
            ..Default::default()
        };
        assert!(table_ddl(f, &designed("KEY", "node", &["id"]), d)
            .unwrap()
            .starts_with("DROP CONSTRAINT ix IF EXISTS;\nCREATE CONSTRAINT"));
    }

    #[test]
    fn memgraph_ddl() {
        let f = Flavor::Memgraph;
        assert_eq!(
            table_ddl(f, &designed("RANGE", "node", &["name"]), parts()).unwrap(),
            "CREATE INDEX ON :Person(name);"
        );
        assert_eq!(
            table_ddl(f, &designed("RANGE", "node", &[]), parts()).unwrap(),
            "CREATE INDEX ON :Person;"
        );
        assert_eq!(
            table_ddl(f, &designed("RANGE", "relationship", &["w"]), parts()).unwrap(),
            "CREATE EDGE INDEX ON :Person(w);"
        );
        assert_eq!(
            table_ddl(f, &designed("EXISTS", "node", &["name"]), parts()).unwrap(),
            "CREATE CONSTRAINT ON (n:Person) ASSERT EXISTS (n.name);"
        );
        let s = IndexSpec {
            name: String::new(),
            target: "L".into(),
            relationship: false,
            kind: "UNIQUE".into(),
            properties: vec!["p".into()],
            options: BTreeMap::new(),
        };
        assert_eq!(
            drop(f, &s, false).unwrap(),
            "DROP CONSTRAINT ON (n:L) ASSERT n.p IS UNIQUE"
        );
        assert!(designer(Flavor::Neptune).is_none());
        // Type constraints and vector indexes, with their settings.
        let opts = |kv: &[(&str, &str)]| {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        let t = IndexSpec {
            kind: "TYPE".into(),
            options: opts(&[(PROPERTY_TYPE, "INTEGER")]),
            ..s.clone()
        };
        assert_eq!(
            create(f, &t, false).unwrap(),
            "CREATE CONSTRAINT ON (n:L) ASSERT n.p IS TYPED INTEGER"
        );
        assert_eq!(
            drop(f, &t, false).unwrap(),
            "DROP CONSTRAINT ON (n:L) ASSERT n.p IS TYPED INTEGER"
        );
        let v = IndexSpec {
            name: "v".into(),
            kind: "VECTOR".into(),
            options: opts(&[("dimension", "2"), ("metric", "cos")]),
            ..s.clone()
        };
        assert_eq!(create(f, &v, false).unwrap(), "CREATE VECTOR INDEX v ON :L(p) WITH CONFIG {\"dimension\": 2, \"metric\": 'cos', \"capacity\": 1000}");
        assert_eq!(drop(f, &v, false).unwrap(), "DROP VECTOR INDEX v");
        let n = Flavor::Neo4j;
        let t = IndexSpec {
            name: "t".into(),
            ..t
        };
        assert_eq!(
            create(n, &t, false).unwrap(),
            "CREATE CONSTRAINT t\nFOR (e:L) REQUIRE e.p IS :: INTEGER"
        );
        assert_eq!(drop(n, &t, false).unwrap(), "DROP CONSTRAINT t");
        let ft = IndexSpec {
            name: "ft".into(),
            kind: "FULLTEXT".into(),
            properties: vec!["a".into(), "b".into()],
            options: opts(&[("fulltext.analyzer", "spanish"), (TARGETS, "A,B")]),
            ..s.clone()
        };
        let ft = IndexSpec::from_index(
            n,
            "A",
            false,
            &IndexDef {
                name: ft.name,
                columns: ft.properties,
                kind: Some(ft.kind),
                options: ft.options,
                ..Default::default()
            },
        );
        assert_eq!(create(n, &ft, false).unwrap(), "CREATE FULLTEXT INDEX ft\nFOR (e:A|B) ON EACH [e.a, e.b]\nOPTIONS {indexConfig: {`fulltext.analyzer`: 'spanish'}}");
        let v = IndexSpec {
            options: opts(&[
                ("vector.dimensions", "4"),
                ("vector.similarity_function", "EUCLIDEAN"),
            ]),
            ..v
        };
        assert_eq!(
            create(n, &v, false).unwrap(),
            "CREATE VECTOR INDEX v\nFOR (e:L) ON (e.p)\nOPTIONS {indexConfig: {`vector.dimensions`: 4, `vector.similarity_function`: 'EUCLIDEAN'}}"
        );
    }

    #[test]
    fn label_scripts() {
        let t = TableSchema {
            kind: LABEL.into(),
            name: "Person".into(),
            indexes: vec![IndexDef {
                name: "u".into(),
                columns: vec!["id".into()],
                unique: true,
                kind: Some("UNIQUE".into()),
                filter: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        let all = DdlParts {
            create: true,
            indexes: true,
            ..Default::default()
        };
        let s = table_ddl(Flavor::Neo4j, &t, all).unwrap();
        assert!(s.starts_with("// Etiqueta Person"), "{s}");
        assert!(
            s.ends_with("CREATE CONSTRAINT u\nFOR (e:Person) REQUIRE e.id IS UNIQUE;"),
            "{s}"
        );
        // A line break in the label can't end the comment.
        let t = TableSchema {
            kind: LABEL.into(),
            name: "P\nMATCH (n) DETACH DELETE n\u{2028}".into(),
            ..Default::default()
        };
        let s = table_ddl(Flavor::Neo4j, &t, all).unwrap();
        assert!(
            s.starts_with("// Etiqueta `P?MATCH (n) DETACH DELETE n?`: en Cypher"),
            "{s}"
        );
        assert_eq!(s.lines().count(), 1, "{s}");
    }

    #[test]
    fn inserts() {
        let target = ObjectRef {
            kind: LABEL.into(),
            schema: None,
            name: "Person".into(),
        };
        let node = json!({ "~id": "4:a:1", "~entityType": "node", "~labels": ["Person", "Dev"], "~properties": { "name": "O'Neil", "tags": ["a"], "meta": { "x": 1 } } });
        let s = insert_script(
            Flavor::Neo4j,
            &target,
            &["n".into()],
            &[vec![Value::String(node.to_string())]],
        )
        .unwrap();
        assert_eq!(
            s,
            "CREATE (:Person:Dev {meta: '{\"x\":1}', name: 'O\\'Neil', tags: ['a']});\n"
        );
        let s = insert_script(
            Flavor::Memgraph,
            &target,
            &["name".into(), "age".into()],
            &[vec![json!("Ann"), Value::Null]],
        )
        .unwrap();
        assert_eq!(s, "CREATE (:Person {name: 'Ann'});\n");
        let rel = json!({ "~id": 5, "~entityType": "relationship", "~type": "KNOWS", "~start": 1, "~end": 2, "~properties": {} });
        let t = ObjectRef {
            kind: RELATIONSHIP.into(),
            schema: None,
            name: "KNOWS".into(),
        };
        let s = insert_script(Flavor::Memgraph, &t, &["r".into()], &[vec![rel]]).unwrap();
        assert_eq!(
            s,
            "MATCH (a), (b) WHERE id(a) = 1 AND id(b) = 2\nCREATE (a)-[:KNOWS]->(b);\n"
        );
        assert!(insert_script(Flavor::Neo4j, &t, &["x".into()], &[vec![json!(1)]]).is_err());
        let rel = json!({ "~entityType": "relationship", "~type": "K", "~start": "7", "~end": "8", "~properties": {} });
        assert!(
            insert_script(Flavor::Memgraph, &t, &["r".into()], &[vec![rel.clone()]])
                .unwrap()
                .contains("id(a) = 7 AND id(b) = 8")
        );
        assert!(
            insert_script(Flavor::Neptune, &t, &["r".into()], &[vec![rel]])
                .unwrap()
                .contains("id(a) = '7'")
        );
    }

    #[test]
    fn updates() {
        let target = ObjectRef {
            kind: LABEL.into(),
            schema: None,
            name: "Person".into(),
        };
        let changes = vec![
            RowChange {
                key: vec![("id".into(), json!(1)), ("nick".into(), Value::Null)],
                set: vec![
                    ("name".into(), json!("O'Brien \"Bob\"")),
                    ("age".into(), Value::Null),
                ],
                ..Default::default()
            },
            RowChange {
                key: vec![("id".into(), json!(2))],
                set: vec![],
                ..Default::default()
            },
        ];
        let s = update_script(Flavor::Neo4j, &target, &changes).unwrap();
        assert_eq!(s, "MATCH (n:Person) WHERE n.id = 1 AND n.nick IS NULL\nSET n.name = 'O\\'Brien \"Bob\"', n.age = null;\n");
        assert_eq!(crate::cypher::split(&s).len(), 1);

        let node = json!({ "~id": "4:a:1", "~entityType": "node", "~labels": ["Person"], "~properties": { "name": "A" } });
        let edited = json!({ "~id": "4:a:1", "~entityType": "node", "~labels": ["Person"], "~properties": { "name": "B", "x": null, "t": ["a"] } });
        let whole = vec![RowChange {
            key: vec![("n".into(), Value::String(node.to_string()))],
            set: vec![("n".into(), Value::String(edited.to_string()))],
            ..Default::default()
        }];
        assert_eq!(
            update_script(Flavor::Neo4j, &target, &whole).unwrap(),
            "MATCH (n) WHERE elementId(n) = '4:a:1'\nSET n = {name: 'B', t: ['a']};\n"
        );
        let rel = json!({ "~id": 5, "~entityType": "relationship", "~type": "K", "~start": 1, "~end": 2, "~properties": {} });
        let t = ObjectRef {
            kind: RELATIONSHIP.into(),
            schema: None,
            name: "K".into(),
        };
        let whole = vec![RowChange {
            key: vec![("r".into(), rel.clone())],
            set: vec![("r".into(), rel)],
            ..Default::default()
        }];
        assert_eq!(
            update_script(Flavor::Memgraph, &t, &whole).unwrap(),
            "MATCH ()-[r]->() WHERE id(r) = 5\nSET r = {};\n"
        );
        let bad = vec![RowChange {
            key: vec![("n".into(), Value::String(node.to_string()))],
            set: vec![("n".into(), json!("x"))],
            ..Default::default()
        }];
        assert!(update_script(Flavor::Neo4j, &target, &bad).is_err());
    }

    #[test]
    fn deletes() {
        let target = ObjectRef {
            kind: LABEL.into(),
            schema: None,
            name: "Person".into(),
        };
        let keys = vec![vec![
            ("id".into(), json!("O'Brien")),
            ("nick".into(), Value::Null),
        ]];
        let s = delete_script(Flavor::Neo4j, &target, &keys).unwrap();
        assert_eq!(
            s,
            "MATCH (n:Person) WHERE n.id = 'O\\'Brien' AND n.nick IS NULL\nDETACH DELETE n;\n"
        );
        assert_eq!(crate::cypher::split(&s).len(), 1);
        assert!(delete_script(Flavor::Neo4j, &target, &[vec![]]).is_err());

        let node = json!({ "~id": "4:a:1", "~entityType": "node", "~labels": ["Person"], "~properties": { "name": "A" } });
        let whole = vec![vec![("n".into(), Value::String(node.to_string()))]];
        assert_eq!(
            delete_script(Flavor::Neo4j, &target, &whole).unwrap(),
            "MATCH (n) WHERE elementId(n) = '4:a:1'\nDETACH DELETE n;\n"
        );
        let rel = json!({ "~id": 5, "~entityType": "relationship", "~type": "K", "~start": 1, "~end": 2, "~properties": {} });
        let t = ObjectRef {
            kind: RELATIONSHIP.into(),
            schema: None,
            name: "K".into(),
        };
        assert_eq!(
            delete_script(Flavor::Memgraph, &t, &[vec![("r".into(), rel)]]).unwrap(),
            "MATCH ()-[r]->() WHERE id(r) = 5\nDELETE r;\n"
        );
        assert_eq!(
            delete_script(Flavor::Neo4j, &t, &[vec![("since".into(), json!(2020))]]).unwrap(),
            "MATCH ()-[r:K]->() WHERE r.since = 2020\nDELETE r;\n"
        );
    }
}
