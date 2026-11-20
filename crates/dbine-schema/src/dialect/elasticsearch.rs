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
//! - Vectors: `dense_vector(n)` (Elasticsearch, with `byte`, `bit` or
//!   `bfloat16` elements) and `knn_vector(n)` (OpenSearch with `byte` or
//!   `binary`; Open Distro, float only), as the driver spells them. A vector
//!   index is the field's ANN index (HNSW, or Elasticsearch's `flat`):
//!   other engines' algorithms become HNSW, and their inner product the
//!   one that takes any vector (`max_inner_product`, not `dot_product`,
//!   which needs unit-length vectors). Every other index is left out: each
//!   field is indexed by its mapping.

use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::vector::param;
use dbine_driver::{TableSchema, VectorElement, VectorIndex};

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
    static ES: Search = Search {
        flavor: Flavor::Elastic,
    };
    static OS: Search = Search {
        flavor: Flavor::OpenSearch,
    };
    static OD: Search = Search {
        flavor: Flavor::OpenDistro,
    };
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
        "text" | "match_only_text" | "search_as_you_type" | "completion" | "wildcard"
        | "keyword" | "constant_keyword" | "version" => L::Text { unicode: true },
        // Text whose embeddings Elasticsearch computes itself: the text is
        // what other engines can take.
        "semantic_text" => L::Text { unicode: true },
        "long" => L::int(8),
        "integer" | "token_count" => L::int(4),
        "short" => L::int(2),
        "byte" => L::int(1),
        "unsigned_long" => L::Int {
            bytes: 8,
            unsigned: true,
        },
        "double" => L::Float { bytes: 8 },
        "float" | "half_float" | "rank_feature" => L::Float { bytes: 4 },
        // A long divided by the scaling factor: exact at that scale.
        "scaled_float" => L::Decimal {
            precision: None,
            scale: None,
        },
        "boolean" => L::Bool,
        "date" => L::Timestamp {
            precision: Some(3),
            tz: true,
        },
        "date_nanos" => L::Timestamp {
            precision: Some(9),
            tz: true,
        },
        "ip" => L::Inet,
        "binary" => L::Blob,
        "geo_point" => L::Geometry {
            kind: Some("point".into()),
            srid: Some(4326),
            geography: true,
        },
        "geo_shape" => L::Geometry {
            kind: None,
            srid: Some(4326),
            geography: true,
        },
        // `rank_vectors` holds several vectors per document: an array of
        // arrays, not one vector.
        "object" | "nested" | "flattened" | "flat_object" | "sparse_vector" | "rank_features"
        | "rank_vectors" | "histogram" | "percolator" | "integer_range" | "long_range"
        | "float_range" | "double_range" | "date_range" | "ip_range" => L::Json { binary: true },
        _ => L::Other {
            native: name.to_string(),
        },
    }
}

/// The vector element named by a type's second argument.
fn element(arg: Option<&String>) -> VectorElement {
    match arg.map(|a| a.trim().to_ascii_lowercase()).as_deref() {
        None | Some("" | "float" | "float32") => VectorElement::F32,
        Some("byte") => VectorElement::I8,
        Some("bit" | "binary") => VectorElement::Bit,
        Some("bfloat16") => VectorElement::Bf16,
        Some(_) => VectorElement::Other,
    }
}

impl Search {
    /// The engine's name, for notes.
    fn engine(&self) -> &'static str {
        match self.flavor {
            Flavor::Elastic => "Elasticsearch",
            Flavor::OpenSearch => "OpenSearch",
            Flavor::OpenDistro => "Open Distro",
        }
    }

    /// The most dimensions a vector field of this flavor takes.
    fn max_dims(&self) -> u32 {
        match self.flavor {
            Flavor::Elastic => 4096,
            Flavor::OpenSearch => 16_000,
            Flavor::OpenDistro => 10_000,
        }
    }

    /// The vector type of this flavor for a logical vector.
    fn render_vector(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let L::Vector {
            dims,
            element,
            sparse,
        } = t
        else {
            return Rendered::exact("float");
        };
        let base = match self.flavor {
            Flavor::Elastic => "dense_vector",
            _ => "knn_vector",
        };
        let max = self.max_dims();
        let engine = self.engine();
        if *sparse {
            return super::vector_fallback(self, t);
        }
        let Some(n) = *dims else {
            if self.flavor == Flavor::Elastic {
                return Rendered::exact("dense_vector").with(
                    Info,
                    TypeChanged,
                    "Vector sin dimensión fija: Elasticsearch la toma del primer documento.",
                );
            }
            return super::vector_fallback(self, t).with(
                Warning,
                TypeApproximated,
                format!("{engine} necesita la dimensión del vector."),
            );
        };
        if n > max {
            return super::vector_fallback(self, t).with(
                Warning,
                TypeApproximated,
                format!("{engine} admite vectores de hasta {max} dimensiones."),
            );
        }
        let float = || Rendered::exact(format!("{base}({n})"));
        match element {
            VectorElement::F32 | VectorElement::Other => float(),
            VectorElement::F64 => float().with(Loss, PrecisionLoss, format!("{engine} guarda los vectores en float de 32 bits: los de 64 bits pierden precisión.")),
            VectorElement::F16 | VectorElement::Bf16 => float().with(Info, TypeChanged, "Vector de media precisión como float de 32 bits."),
            VectorElement::I8 if self.flavor != Flavor::OpenDistro => Rendered::exact(format!("{base}({n}, byte)")),
            VectorElement::Bit if self.flavor != Flavor::OpenDistro && n.is_multiple_of(8) => Rendered::exact(format!(
                "{base}({n}, {})",
                if self.flavor == Flavor::Elastic { "bit" } else { "binary" }
            )),
            VectorElement::Bit if self.flavor == Flavor::OpenDistro => float().with(Warning, TypeApproximated, "Vector de bits como números (0 y 1): Open Distro no tiene vectores de bits."),
            VectorElement::Bit => float().with(Warning, TypeApproximated, format!("Vector de bits como números (0 y 1): {engine} guarda bits solo de a 8 dimensiones.")),
            VectorElement::I8 | VectorElement::U8 => float().with(Info, TypeChanged, "Vector de bytes como números float."),
        }
    }

    /// The dimension and element of the vector field a column of logical
    /// type `column` is on this flavor, or why it isn't one (it renders as
    /// a list of numbers).
    fn target_vector(&self, column: &L) -> Result<(Option<u32>, VectorElement), String> {
        let engine = self.engine();
        let L::Vector { dims, sparse, .. } = column else {
            return Err(format!("su columna no es un vector en {engine}"));
        };
        let rendered = self.render_type(column);
        match self.parse_type(&crate::parse::parse(&rendered.native)) {
            L::Vector { dims, element, .. } => Ok((dims, element)),
            _ if *sparse => Err(format!(
                "{engine} no tiene índices vectoriales de vectores dispersos"
            )),
            _ if dims.is_none() => Err(format!("{engine} necesita la dimensión del vector")),
            _ if dims.is_some_and(|d| d > self.max_dims()) => Err(format!(
                "{engine} admite vectores de hasta {} dimensiones",
                self.max_dims()
            )),
            _ => Err(format!("su columna no es un vector en {engine}")),
        }
    }

    /// Takes OpenSearch's own index settings (`index.replication.*`,
    /// `index.remote_store.*`) out of the index's other settings, as the
    /// driver reads them (`settings_extra`), each one reported.
    fn drop_opensearch_settings(&self, t: &mut TableSchema, table: &str, report: &mut Report) {
        const KEY: &str = "settings_extra";
        let Some(serde_json::Value::Object(mut extra)) = t
            .options
            .get(KEY)
            .and_then(|s| serde_json::from_str(s).ok())
        else {
            return;
        };
        let own = |k: &str| {
            let rel = k.strip_prefix("index.").unwrap_or(k);
            rel.starts_with("replication.") || rel.starts_with("remote_store.")
        };
        let gone: Vec<String> = extra.keys().filter(|k| own(k)).cloned().collect();
        if gone.is_empty() {
            return;
        }
        for k in &gone {
            extra.remove(k);
            report.push(
                Severity::Info,
                IssueCode::OptionDropped,
                table,
                None,
                format!(
                    "{} no tiene el ajuste «{k}» de OpenSearch: se quita.",
                    self.engine()
                ),
            );
        }
        if extra.is_empty() {
            t.options.remove(KEY);
        } else {
            t.options
                .insert(KEY.into(), serde_json::Value::Object(extra).to_string());
        }
    }

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
        if matches!(t.name.as_str(), "dense_vector" | "knn_vector") {
            return L::Vector {
                dims: t.arg_u32(0),
                element: element(t.args.get(1)),
                sparse: false,
            };
        }
        match field_type(&t.name) {
            L::Other { .. } => L::Other {
                native: t.raw.clone(),
            },
            l => l,
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Vector { .. } => self.render_vector(t),
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
            // Every field is indexed by itself: the indexes are the vector
            // fields' ANN indexes (see `finalize`).
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: false,
            nullability: false,
            comments: true,
            max_identifier: 255,
            case: IdentCase::Lower,
            vector_indexes: true,
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
            if let Some(f) = ty
                .strip_prefix("scaled_float(")
                .and_then(|r| r.strip_suffix(')'))
            {
                c.options.insert("scaling_factor".into(), f.to_string());
                c.data_type = "scaled_float".into();
            } else if (ty == "date" || ty == "date_nanos") && !c.options.contains_key("format") {
                c.options.insert("format".into(), DATE_FORMAT.into());
            }
        }
        // OpenSearch writes its replication type into every index's
        // settings (and remote-backed storage there too): Elasticsearch
        // and Open Distro refuse an index with settings they don't know.
        if self.flavor != Flavor::OpenSearch {
            self.drop_opensearch_settings(t, &source_name, report);
        }
        // A vector from another of these flavors (one dialect, so its type
        // came as it was): this flavor's vector type, and its index in
        // this flavor's terms. Open Distro checks its own spelling too:
        // OpenSearch's `knn_vector` may be byte or binary, or have more
        // dimensions than Open Distro takes, and its index may use what
        // Open Distro lacks; so on Open Distro every vector index is
        // mapped (its own settings map to themselves).
        let own = match self.flavor {
            Flavor::Elastic => "dense_vector",
            _ => "knn_vector",
        };
        let mut moved: Vec<(String, L)> = Vec::new();
        for c in &mut t.columns {
            let spec = crate::parse::parse(&c.data_type);
            if !matches!(spec.name.as_str(), "dense_vector" | "knn_vector")
                || (spec.name == own && self.flavor != Flavor::OpenDistro)
            {
                continue;
            }
            let source = self.parse_type(&spec);
            let r = self.render_type(&source);
            if spec.name != own || self.parse_type(&crate::parse::parse(&r.native)) != source {
                for n in r.notes {
                    report.push(n.severity, n.code, &source_name, Some(&c.name), n.message);
                }
                c.data_type = r.native;
            }
            moved.push((c.name.clone(), source));
        }
        for ix in &mut t.indexes {
            let column = ix
                .columns
                .first()
                .and_then(|c| moved.iter().find(|(m, _)| m == c))
                .map(|(_, l)| l);
            let Some((v, column)) = ix.vector.as_ref().zip(column) else {
                continue;
            };
            match self.vector_index(v, Some(column)) {
                Ok((kind, v)) => {
                    ix.kind = kind;
                    ix.vector = Some(v);
                    ix.options.clear();
                }
                Err(why) => {
                    report.push(
                        Severity::Dropped,
                        IssueCode::IndexDropped,
                        &source_name,
                        Some(&ix.name),
                        format!("El índice vectorial «{}» no se crea: {why}.", ix.name),
                    );
                    ix.vector = None;
                    ix.name.clear();
                }
            }
        }
        t.indexes
            .retain(|ix| ix.vector.is_some() || !ix.name.is_empty());
        // Only the vector fields' ANN indexes, one per field.
        let vector_field = |t: &TableSchema, col: &str| {
            t.columns.iter().any(|c| {
                c.name == col && {
                    let ty = c.data_type.trim();
                    ty.starts_with("dense_vector") || ty.starts_with("knn_vector")
                }
            })
        };
        let mut kept: Vec<dbine_driver::IndexDef> = Vec::new();
        for ix in std::mem::take(&mut t.indexes) {
            let col = ix.columns.first().cloned().unwrap_or_default();
            let why = if ix.vector.is_none() {
                Some((
                    Severity::Info,
                    "no hace falta: cada campo se indexa por su propio mapping".to_string(),
                ))
            } else if ix.columns.len() != 1 || !vector_field(t, &col) {
                Some((Severity::Dropped, format!("{} lo crea sobre un campo vectorial solo, y «{col}» no lo es en el destino", self.engine())))
            } else if kept.iter().any(|k| k.columns[0] == col) {
                Some((
                    Severity::Dropped,
                    format!("«{col}» ya tiene su índice vectorial"),
                ))
            } else {
                // Open Distro's distance and HNSW parameters are the whole
                // index's settings: the first vector index sets them.
                kept.first()
                    .filter(|k| self.flavor == Flavor::OpenDistro && od_settings(k) != od_settings(&ix))
                    .map(|first| (
                        Severity::Dropped,
                        format!(
                            "en Open Distro la distancia y los parámetros k-NN son de todo el índice, y ya los fija «{}»",
                            first.name
                        ),
                    ))
            };
            match why {
                Some((sev, why)) => report.push(
                    sev,
                    IssueCode::IndexDropped,
                    &source_name,
                    Some(&ix.name),
                    format!("El índice «{}» no se crea: {why}.", ix.name),
                ),
                None => kept.push(dbine_driver::IndexDef {
                    unique: false,
                    filter: None,
                    ..ix
                }),
            }
        }
        t.indexes = kept;
    }

    /// Another engine's vector index as a field's ANN index: HNSW (or
    /// Elasticsearch's `flat`) with `m` and `ef_construction`; the inner
    /// product that takes any vector (`max_inner_product`). With the
    /// column's type, what its vector field is here decides: bit (binary)
    /// vectors are searched by Hamming (their L2 ranks the same) and only
    /// they are; OpenSearch searches bytes with Lucene, which has no L1;
    /// Elasticsearch quantizes only floats, int4 an even dimension and BBQ
    /// from 64. A column that isn't a vector field here has none.
    fn vector_index(
        &self,
        v: &VectorIndex,
        column: Option<&crate::logical::LogicalType>,
    ) -> Result<(Option<String>, VectorIndex), String> {
        let engine = self.engine();
        let target = column.map(|c| self.target_vector(c)).transpose()?;
        let bits = target.is_some_and(|(_, e)| e == VectorElement::Bit);
        let bytes = target.is_some_and(|(_, e)| e == VectorElement::I8);
        let metric =
            match v.metric.as_deref().map(str::to_ascii_lowercase).as_deref() {
                None => None,
                Some("hamming" | "l2") if bits => Some("hamming"),
                Some(m) if bits => {
                    return Err(format!(
                    "{engine} busca los vectores de bits solo por distancia de Hamming, no «{m}»"
                ))
                }
                Some("hamming") if target.is_some() => {
                    return Err(format!(
                        "en {engine} la distancia de Hamming es solo de vectores de bits"
                    ))
                }
                Some("l1") if bytes && self.flavor == Flavor::OpenSearch => return Err(
                    "OpenSearch busca los vectores byte con Lucene, que no tiene la distancia «l1»"
                        .into(),
                ),
                Some("cosine") => Some("cosine"),
                Some("l2") => Some("l2"),
                Some("ip") if self.flavor == Flavor::Elastic => Some("max_inner_product"),
                Some("ip") if self.flavor == Flavor::OpenSearch => Some("ip"),
                Some("hamming") if self.flavor != Flavor::OpenDistro => Some("hamming"),
                Some("l1") if self.flavor != Flavor::Elastic => Some("l1"),
                Some(m) => {
                    return Err(format!(
                        "{engine} no tiene la distancia «{m}» en sus índices vectoriales"
                    ))
                }
            };
        let algorithm = match v
            .algorithm
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("flat") if self.flavor == Flavor::Elastic => "flat",
            _ => "hnsw",
        };
        // HNSW's parameters only mean something to another HNSW.
        let hnsw_like = v
            .algorithm
            .as_deref()
            .is_none_or(|a| a.to_ascii_lowercase().contains("hnsw"));
        let mut params = std::collections::BTreeMap::new();
        if hnsw_like && algorithm == "hnsw" {
            for k in [param::M, param::EF_CONSTRUCTION] {
                if let Some(x) = v.params.get(k) {
                    params.insert(k.to_string(), x.clone());
                }
            }
        }
        if self.flavor == Flavor::Elastic {
            // Quantized floats only: int4 needs an even dimension, BBQ 64
            // or more.
            let fits = |q: &str| match target {
                None => true,
                Some((dims, e)) => {
                    matches!(e, VectorElement::F32 | VectorElement::Bf16)
                        && match q {
                            "int4" => dims.is_some_and(|d| d.is_multiple_of(2)),
                            "bbq" => dims.is_some_and(|d| d >= 64),
                            _ => true,
                        }
                }
            };
            if let Some(q) = v
                .params
                .get(param::QUANTIZATION)
                .map(|q| q.to_ascii_lowercase())
                .filter(|q| matches!(q.as_str(), "int8" | "int4" | "bbq") && fits(q))
            {
                params.insert(param::QUANTIZATION.to_string(), q);
            }
        }
        Ok((
            None,
            VectorIndex {
                algorithm: Some(algorithm.to_string()),
                metric: metric.map(str::to_string),
                params,
            },
        ))
    }
}

/// What an Open Distro vector index sets for its whole index: the distance
/// (`l2` when none is named, the engine's default) and the HNSW
/// parameters.
fn od_settings(ix: &dbine_driver::IndexDef) -> (String, Vec<(String, String)>) {
    let v = ix.vector.clone().unwrap_or_default();
    let metric = v
        .metric
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "l2".into());
    let params = v
        .params
        .into_iter()
        .map(|(k, x)| (k, x.trim().to_string()))
        .filter(|(_, x)| !x.is_empty())
        .collect();
    (metric, params)
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
        assert_eq!(
            p("unsigned_long"),
            L::Int {
                bytes: 8,
                unsigned: true
            }
        );
        assert_eq!(
            p("scaled_float"),
            L::Decimal {
                precision: None,
                scale: None
            }
        );
        assert_eq!(p("half_float"), L::Float { bytes: 4 });
        assert_eq!(
            p("date"),
            L::Timestamp {
                precision: Some(3),
                tz: true
            }
        );
        assert_eq!(
            p("date_nanos"),
            L::Timestamp {
                precision: Some(9),
                tz: true
            }
        );
        assert_eq!(p("ip"), L::Inet);
        assert_eq!(p("object"), L::Json { binary: true });
        assert_eq!(p("nested"), L::Json { binary: true });
        assert!(matches!(p("geo_point"), L::Geometry { .. }));
        assert!(matches!(p("alias"), L::Other { .. }));
        assert_eq!(p("semantic_text"), L::Text { unicode: true });
        assert_eq!(p("rank_vectors(128)"), L::Json { binary: true });
        assert_eq!(p("rank_vectors(128, byte)"), L::Json { binary: true });
    }

    #[test]
    fn renders_every_variant() {
        let d = lookup("elasticsearch").unwrap();
        let r = |t: L| d.render_type(&t).native;
        assert_eq!(r(L::Bool), "boolean");
        assert_eq!(r(L::int(2)), "short");
        assert_eq!(r(L::int(16)), "double");
        assert_eq!(
            r(L::Int {
                bytes: 4,
                unsigned: true
            }),
            "long"
        );
        assert_eq!(
            r(L::Int {
                bytes: 8,
                unsigned: true
            }),
            "unsigned_long"
        );
        assert_eq!(
            lookup("opendistro")
                .unwrap()
                .render_type(&L::Int {
                    bytes: 8,
                    unsigned: true
                })
                .native,
            "double"
        );
        assert_eq!(
            r(L::Decimal {
                precision: Some(12),
                scale: Some(2)
            }),
            "scaled_float(100)"
        );
        assert_eq!(
            r(L::Decimal {
                precision: Some(12),
                scale: Some(0)
            }),
            "long"
        );
        assert_eq!(
            r(L::Decimal {
                precision: None,
                scale: None
            }),
            "double"
        );
        assert_eq!(r(L::Float { bytes: 4 }), "float");
        assert_eq!(r(L::Money), "scaled_float(10000)");
        assert_eq!(
            r(L::Char {
                len: Some(2),
                unicode: true
            }),
            "keyword"
        );
        assert_eq!(
            r(L::Varchar {
                len: Some(256),
                unicode: true
            }),
            "keyword"
        );
        assert_eq!(
            r(L::Varchar {
                len: Some(257),
                unicode: true
            }),
            "text"
        );
        assert_eq!(
            r(L::Varchar {
                len: None,
                unicode: true
            }),
            "text"
        );
        assert_eq!(r(L::Text { unicode: true }), "text");
        assert_eq!(r(L::Blob), "binary");
        assert_eq!(r(L::Bit { len: None }), "keyword");
        assert_eq!(r(L::Date), "date");
        assert_eq!(
            r(L::Time {
                precision: None,
                tz: false
            }),
            "keyword"
        );
        assert_eq!(
            r(L::Timestamp {
                precision: Some(3),
                tz: true
            }),
            "date"
        );
        assert_eq!(
            r(L::Timestamp {
                precision: Some(6),
                tz: true
            }),
            "date_nanos"
        );
        assert_eq!(r(L::Interval), "keyword");
        assert_eq!(r(L::Year), "short");
        assert_eq!(r(L::Uuid), "keyword");
        assert_eq!(r(L::Json { binary: true }), "flattened");
        assert_eq!(
            lookup("opensearch")
                .unwrap()
                .render_type(&L::Json { binary: true })
                .native,
            "flat_object"
        );
        assert_eq!(r(L::Xml), "text");
        assert_eq!(r(L::Enum { values: vec![] }), "keyword");
        assert_eq!(r(L::Set { values: vec![] }), "keyword");
        assert_eq!(
            r(L::Array {
                of: Box::new(L::int(4))
            }),
            "integer"
        );
        assert_eq!(
            r(L::Map {
                key: Box::new(L::Bool),
                value: Box::new(L::Bool)
            }),
            "flattened"
        );
        assert_eq!(
            r(L::Geometry {
                kind: Some("point".into()),
                srid: None,
                geography: false
            }),
            "geo_point"
        );
        assert_eq!(
            r(L::Geometry {
                kind: None,
                srid: None,
                geography: false
            }),
            "geo_shape"
        );
        assert_eq!(r(L::Inet), "ip");
        assert_eq!(r(L::MacAddr), "keyword");
        assert_eq!(r(L::RowVersion), "binary");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
    }

    fn vec3(element: VectorElement) -> L {
        L::Vector {
            dims: Some(3),
            element,
            sparse: false,
        }
    }

    #[test]
    fn vectors_parse_and_render() {
        let es = lookup("elasticsearch").unwrap();
        let os = lookup("opensearch").unwrap();
        let od = lookup("opendistro").unwrap();
        let p = |d: &dyn Dialect, s: &str| d.parse_type(&parse(s));
        assert_eq!(p(es, "dense_vector(3)"), vec3(VectorElement::F32));
        assert_eq!(p(es, "dense_vector(3, byte)"), vec3(VectorElement::I8));
        assert_eq!(p(es, "dense_vector(3, bit)"), vec3(VectorElement::Bit));
        assert_eq!(
            p(es, "dense_vector(3, bfloat16)"),
            vec3(VectorElement::Bf16)
        );
        assert_eq!(
            p(es, "dense_vector"),
            L::Vector {
                dims: None,
                element: VectorElement::F32,
                sparse: false
            }
        );
        assert_eq!(p(os, "knn_vector(3, binary)"), vec3(VectorElement::Bit));
        assert_eq!(p(os, "knn_vector(3)"), vec3(VectorElement::F32));
        assert_eq!(p(es, "sparse_vector"), L::Json { binary: true });
        // Round trips, every flavor.
        for (d, spellings) in [
            (
                es,
                &[
                    "dense_vector(3)",
                    "dense_vector(3, byte)",
                    "dense_vector(16, bit)",
                    "dense_vector",
                ][..],
            ),
            (
                os,
                &[
                    "knn_vector(3)",
                    "knn_vector(3, byte)",
                    "knn_vector(16, binary)",
                ][..],
            ),
            (od, &["knn_vector(3)"][..]),
        ] {
            for s in spellings {
                let r = d.render_type(&p(d, s));
                assert_eq!(r.native, *s, "{s}");
                assert!(
                    r.notes.is_empty() || *s == "dense_vector",
                    "{s}: {:?}",
                    r.notes
                );
            }
        }
        // pgvector's vector(3) (a float32 vector of 3) and halfvec / bit.
        let r = |d: &dyn Dialect, t: L| d.render_type(&t);
        assert_eq!(r(es, vec3(VectorElement::F32)).native, "dense_vector(3)");
        assert_eq!(r(os, vec3(VectorElement::F32)).native, "knn_vector(3)");
        assert_eq!(r(od, vec3(VectorElement::F32)).native, "knn_vector(3)");
        let half = r(es, vec3(VectorElement::F16));
        assert_eq!(
            (half.native.as_str(), half.notes.len()),
            ("dense_vector(3)", 1)
        );
        let f64v = r(es, vec3(VectorElement::F64));
        assert_eq!(f64v.notes[0].severity, Severity::Loss);
        // Bits only by eights (and never on Open Distro); bytes on Open Distro as floats.
        let bits = |n| L::Vector {
            dims: Some(n),
            element: VectorElement::Bit,
            sparse: false,
        };
        assert_eq!(r(es, bits(16)).native, "dense_vector(16, bit)");
        assert_eq!(r(os, bits(16)).native, "knn_vector(16, binary)");
        assert_eq!(r(es, bits(10)).native, "dense_vector(10)");
        assert_eq!(r(od, bits(16)).native, "knn_vector(16)");
        assert_eq!(r(od, vec3(VectorElement::I8)).native, "knn_vector(3)");
        // Too long, or without a dimension where one is needed: a list of floats.
        let long = L::Vector {
            dims: Some(5000),
            element: VectorElement::F32,
            sparse: false,
        };
        assert_eq!(r(es, long.clone()).native, "float");
        assert_eq!(r(os, long).native, "knn_vector(5000)");
        let free = L::Vector {
            dims: None,
            element: VectorElement::F32,
            sparse: false,
        };
        assert_eq!(r(es, free.clone()).native, "dense_vector");
        assert_eq!(r(os, free).native, "float");
        let sparse = L::Vector {
            dims: Some(30000),
            element: VectorElement::F32,
            sparse: true,
        };
        assert_eq!(r(es, sparse).native, "flattened");
    }

    #[test]
    fn foreign_vector_indexes() {
        let es = lookup("elasticsearch").unwrap();
        let os = lookup("opensearch").unwrap();
        let od = lookup("opendistro").unwrap();
        let pg_hnsw = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("ip".into()),
            params: [
                ("m".to_string(), "16".to_string()),
                ("ef_construction".to_string(), "64".to_string()),
            ]
            .into(),
        };
        let (kind, v) = es.vector_index(&pg_hnsw, None).unwrap();
        assert_eq!(kind, None);
        assert_eq!(v.algorithm.as_deref(), Some("hnsw"));
        // Any vector's inner product, not dot_product (unit length only).
        assert_eq!(v.metric.as_deref(), Some("max_inner_product"));
        assert_eq!(v.params, pg_hnsw.params);
        assert_eq!(
            os.vector_index(&pg_hnsw, None).unwrap().1.metric.as_deref(),
            Some("ip")
        );
        assert!(od.vector_index(&pg_hnsw, None).unwrap_err().contains("ip"));
        // IVF and others: HNSW with the engine's parameters.
        let ivf = VectorIndex {
            algorithm: Some("ivfflat".into()),
            metric: Some("l2".into()),
            params: [("lists".to_string(), "100".to_string())].into(),
        };
        let (_, v) = es.vector_index(&ivf, None).unwrap();
        assert_eq!(
            (v.algorithm.as_deref(), v.metric.as_deref()),
            (Some("hnsw"), Some("l2"))
        );
        assert!(v.params.is_empty());
        assert!(es
            .vector_index(
                &VectorIndex {
                    metric: Some("jaccard".into()),
                    ..Default::default()
                },
                None
            )
            .is_err());
        assert!(es
            .vector_index(
                &VectorIndex {
                    metric: Some("l1".into()),
                    ..Default::default()
                },
                None
            )
            .is_err());
        assert_eq!(
            os.vector_index(
                &VectorIndex {
                    metric: Some("l1".into()),
                    ..Default::default()
                },
                None
            )
            .unwrap()
            .1
            .metric
            .as_deref(),
            Some("l1")
        );
    }

    #[test]
    fn vector_indexes_follow_the_column() {
        let es = lookup("elasticsearch").unwrap();
        let os = lookup("opensearch").unwrap();
        let od = lookup("opendistro").unwrap();
        let ix = |metric: &str, params: &[(&str, &str)]| VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some(metric.into()),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let vec = |dims: u32, element| L::Vector {
            dims: Some(dims),
            element,
            sparse: false,
        };
        let metric = |d: &dyn Dialect, v: VectorIndex, c: L| {
            d.vector_index(&v, Some(&c))
                .map(|(_, v)| v.metric.unwrap_or_default())
        };
        // Bits: Hamming (their L2 ranks the same), nothing else; only where
        // the column stays a bit vector (by eights, never on Open Distro).
        let bits = vec(16, VectorElement::Bit);
        for d in [es, os] {
            assert_eq!(
                metric(d, ix("hamming", &[]), bits.clone()).unwrap(),
                "hamming"
            );
            assert_eq!(metric(d, ix("l2", &[]), bits.clone()).unwrap(), "hamming");
            assert!(metric(d, ix("cosine", &[]), bits.clone())
                .unwrap_err()
                .contains("Hamming"));
            assert!(metric(d, ix("hamming", &[]), vec(3, VectorElement::F32)).is_err());
        }
        assert!(metric(es, ix("hamming", &[]), vec(10, VectorElement::Bit)).is_err());
        assert!(metric(od, ix("hamming", &[]), bits.clone()).is_err());
        assert_eq!(metric(od, ix("l2", &[]), bits).unwrap(), "l2");
        // OpenSearch searches bytes with Lucene: no L1 there.
        assert!(metric(os, ix("l1", &[]), vec(3, VectorElement::I8)).is_err());
        assert_eq!(
            metric(os, ix("l1", &[]), vec(3, VectorElement::F32)).unwrap(),
            "l1"
        );
        assert_eq!(
            metric(os, ix("cosine", &[]), vec(3, VectorElement::I8)).unwrap(),
            "cosine"
        );
        // Elasticsearch quantizes floats only, int4 an even dimension, BBQ
        // from 64.
        let quant = |q: &str, c: L| {
            es.vector_index(&ix("cosine", &[("quantization", q)]), Some(&c))
                .unwrap()
                .1
                .params
                .get("quantization")
                .cloned()
        };
        assert_eq!(
            quant("int4", vec(4, VectorElement::F32)).as_deref(),
            Some("int4")
        );
        assert_eq!(quant("int4", vec(3, VectorElement::F32)), None);
        assert_eq!(quant("bbq", vec(32, VectorElement::F32)), None);
        assert_eq!(
            quant("bbq", vec(64, VectorElement::F32)).as_deref(),
            Some("bbq")
        );
        assert_eq!(quant("int8", vec(3, VectorElement::I8)), None);
        assert_eq!(
            quant("int8", vec(3, VectorElement::F16)).as_deref(),
            Some("int8")
        );
        // A column that isn't a vector field here has no vector index.
        let free = L::Vector {
            dims: None,
            element: VectorElement::F32,
            sparse: false,
        };
        assert!(os
            .vector_index(&ix("l2", &[]), Some(&free))
            .unwrap_err()
            .contains("dimensión"));
        assert!(es.vector_index(&ix("l2", &[]), Some(&free)).is_ok());
        assert!(es
            .vector_index(&ix("l2", &[]), Some(&vec(5000, VectorElement::F32)))
            .is_err());
        let sparse = L::Vector {
            dims: Some(100),
            element: VectorElement::F32,
            sparse: true,
        };
        assert!(es
            .vector_index(&ix("ip", &[]), Some(&sparse))
            .unwrap_err()
            .contains("dispersos"));
        assert!(es
            .vector_index(&ix("l2", &[]), Some(&L::Text { unicode: true }))
            .is_err());
    }

    #[test]
    fn foreign_vectors_move_by_their_column() {
        let one = |src: &str, ty: &str, v: VectorIndex, dst: &str| {
            let t = TableSchema {
                kind: "table".into(),
                name: "items".into(),
                columns: vec![dbine_driver::ColumnDef {
                    name: "emb".into(),
                    data_type: ty.into(),
                    ..Default::default()
                }],
                indexes: vec![dbine_driver::IndexDef {
                    name: "items_emb".into(),
                    columns: vec!["emb".into()],
                    vector: Some(v),
                    ..Default::default()
                }],
                ..Default::default()
            };
            crate::convert(&[t], src, dst, &Default::default()).unwrap()
        };
        let l2 = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("l2".into()),
            ..Default::default()
        };
        // MongoDB's bit vectors searched by euclidean distance: Hamming on
        // the bit vectors of Elasticsearch and OpenSearch, L2 on the float
        // ones of Open Distro and Solr.
        for (dst, ty, metric) in [
            ("elasticsearch", "dense_vector(16, bit)", "hamming"),
            ("opensearch", "knn_vector(16, binary)", "hamming"),
            ("opendistro", "knn_vector(16)", "l2"),
            ("solr", "knn_vector(16)", "l2"),
        ] {
            let c = one("mongodb", "vector(16, bit)", l2.clone(), dst);
            let t = &c.tables[0];
            assert_eq!(t.columns[0].data_type, ty, "{dst}");
            assert_eq!(
                t.indexes[0]
                    .vector
                    .as_ref()
                    .and_then(|v| v.metric.as_deref()),
                Some(metric),
                "{dst}: {:?}",
                c.issues
            );
        }
        // By cosine, bits have no vector index on Elasticsearch.
        let cosine = VectorIndex {
            metric: Some("cosine".into()),
            ..l2.clone()
        };
        let c = one("mongodb", "vector(16, bit)", cosine, "elasticsearch");
        assert!(c.tables[0].indexes.is_empty());
        assert!(
            c.issues.iter().any(|i| i.severity == Severity::Dropped
                && i.object.as_deref() == Some("items_emb")
                && i.message.contains("Hamming")),
            "{:?}",
            c.issues
        );
        // A ClickHouse array searched by a vector index that states its
        // dimension: a vector field with its index.
        let ch = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("cosine".into()),
            params: [("dimensions".to_string(), "3".to_string())].into(),
        };
        for (dst, ty) in [
            ("elasticsearch", "dense_vector(3)"),
            ("opensearch", "knn_vector(3)"),
            ("solr", "knn_vector(3)"),
        ] {
            let c = one("clickhouse", "Array(Float32)", ch.clone(), dst);
            let t = &c.tables[0];
            assert_eq!(t.columns[0].data_type, ty, "{dst}");
            assert_eq!(t.indexes.len(), 1, "{dst}: {:?}", c.issues);
        }
        // pgvector's vector(3) with an int4-quantized index: Elasticsearch
        // leaves the quantization to the engine (int4 needs an even
        // dimension).
        let q = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("cosine".into()),
            params: [("quantization".to_string(), "int4".to_string())].into(),
        };
        let c = one("postgres", "vector(3)", q, "elasticsearch");
        let v = c.tables[0].indexes[0].vector.clone().unwrap();
        assert!(!v.params.contains_key("quantization"), "{v:?}");
        // Between these engines too: Elasticsearch's bit vectors (l2_norm
        // is Hamming) keep it on OpenSearch and lose the index on Open
        // Distro, which has no bit vectors.
        let hamming = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("hamming".into()),
            ..Default::default()
        };
        let os = one(
            "elasticsearch",
            "dense_vector(16, bit)",
            hamming.clone(),
            "opensearch",
        );
        assert_eq!(
            os.tables[0].indexes[0]
                .vector
                .as_ref()
                .and_then(|v| v.metric.as_deref()),
            Some("hamming")
        );
        let od = one(
            "elasticsearch",
            "dense_vector(16, bit)",
            hamming,
            "opendistro",
        );
        assert!(od.tables[0].indexes.is_empty());
        assert!(
            od.issues
                .iter()
                .any(|i| i.severity == Severity::Dropped && i.message.contains("Hamming")),
            "{:?}",
            od.issues
        );
    }

    fn table(ty: &str, vector: Option<VectorIndex>) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            name: "Items".into(),
            columns: vec![
                dbine_driver::ColumnDef {
                    name: "id".into(),
                    data_type: "keyword".into(),
                    ..Default::default()
                },
                dbine_driver::ColumnDef {
                    name: "emb".into(),
                    data_type: ty.into(),
                    ..Default::default()
                },
            ],
            indexes: vec![
                dbine_driver::IndexDef {
                    name: "items_emb".into(),
                    columns: vec!["emb".into()],
                    vector,
                    ..Default::default()
                },
                dbine_driver::IndexDef {
                    name: "items_id".into(),
                    columns: vec!["id".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn vector_columns_and_indexes_move_between_search_engines() {
        let cosine = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("cosine".into()),
            params: [("m".to_string(), "24".to_string())].into(),
        };
        // Elasticsearch → Solr: the vector, its index; the plain one is left out.
        let c = crate::convert(
            &[table("dense_vector(3)", Some(cosine.clone()))],
            "elasticsearch",
            "solr",
            &Default::default(),
        )
        .unwrap();
        let t = &c.tables[0];
        assert_eq!(t.columns[1].data_type, "knn_vector(3)");
        assert_eq!(t.indexes.len(), 1, "{:?}", t.indexes);
        assert_eq!(t.indexes[0].vector, Some(cosine.clone()));
        assert!(
            c.issues
                .iter()
                .any(|i| i.object.as_deref() == Some("items_id") && i.severity == Severity::Info),
            "{:?}",
            c.issues
        );
        // Solr → OpenSearch and back to Elasticsearch: still the same.
        let o = crate::convert(&c.tables, "solr", "opensearch", &Default::default()).unwrap();
        assert_eq!(o.tables[0].columns[1].data_type, "knn_vector(3)");
        assert_eq!(o.tables[0].indexes[0].vector, Some(cosine.clone()));
        let e = crate::convert(
            &o.tables,
            "opensearch",
            "elasticsearch",
            &Default::default(),
        )
        .unwrap();
        assert_eq!(e.tables[0].columns[1].data_type, "dense_vector(3)");
        assert_eq!(e.tables[0].indexes[0].vector, Some(cosine.clone()));
        // A vector the target can't hold as one: its index is left out.
        let c = crate::convert(
            &[table("dense_vector(3, bit)", Some(cosine))],
            "elasticsearch",
            "opendistro",
            &Default::default(),
        )
        .unwrap();
        assert_eq!(c.tables[0].columns[1].data_type, "knn_vector(3)");
        let c = crate::convert(
            &[table("dense_vector", Some(VectorIndex::default()))],
            "elasticsearch",
            "solr",
            &Default::default(),
        )
        .unwrap();
        assert!(!c.tables[0].columns[1].data_type.starts_with("knn_vector"));
        assert!(c.tables[0].indexes.is_empty());
        assert!(
            c.issues.iter().any(
                |i| i.object.as_deref() == Some("items_emb") && i.severity == Severity::Dropped
            ),
            "{:?}",
            c.issues
        );
    }

    #[test]
    fn opensearch_vectors_move_to_open_distro_as_it_takes_them() {
        let index =
            |name: &str, col: &str, metric: &str, params: &[(&str, &str)]| dbine_driver::IndexDef {
                name: name.into(),
                columns: vec![col.into()],
                vector: Some(VectorIndex {
                    algorithm: Some("hnsw".into()),
                    metric: Some(metric.into()),
                    params: params
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                }),
                ..Default::default()
            };
        let one = |ty: &str, ix: dbine_driver::IndexDef, dst: &str| {
            let mut t = table(ty, None);
            t.indexes = vec![ix];
            crate::convert(&[t], "opensearch", dst, &Default::default()).unwrap()
        };
        let dropped = |c: &crate::Conversion, why: &str| {
            c.issues.iter().any(|i| {
                i.severity == Severity::Dropped
                    && i.object.as_deref() == Some("items_emb")
                    && i.message.contains(why)
            })
        };
        // Binary vectors: Open Distro has floats only, and Hamming only
        // for bits.
        let c = one(
            "knn_vector(16, binary)",
            index("items_emb", "emb", "hamming", &[("engine", "faiss")]),
            "opendistro",
        );
        let t = &c.tables[0];
        assert_eq!(t.columns[1].data_type, "knn_vector(16)");
        assert!(t.indexes.is_empty(), "{:?}", t.indexes);
        assert!(dropped(&c, "Hamming"), "{:?}", c.issues);
        assert!(
            c.issues
                .iter()
                .any(|i| i.object.as_deref() == Some("emb") && i.severity == Severity::Warning),
            "{:?}",
            c.issues
        );
        // Byte vectors become floats and keep their index, in Open
        // Distro's terms (no engine).
        let c = one(
            "knn_vector(3, byte)",
            index(
                "items_emb",
                "emb",
                "l2",
                &[("engine", "lucene"), ("m", "8")],
            ),
            "opendistro",
        );
        let t = &c.tables[0];
        assert_eq!(t.columns[1].data_type, "knn_vector(3)");
        assert_eq!(
            t.indexes[0].vector,
            Some(VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some("l2".into()),
                params: [("m".to_string(), "8".to_string())].into(),
            })
        );
        // More dimensions than Open Distro takes: not a vector there.
        let c = one(
            "knn_vector(12000)",
            index("items_emb", "emb", "l2", &[]),
            "opendistro",
        );
        assert!(!c.tables[0].columns[1].data_type.starts_with("knn_vector"));
        assert!(c.tables[0].indexes.is_empty());
        assert!(dropped(&c, "10000 dimensiones"), "{:?}", c.issues);
        // A float vector keeps its type; its index loses what Open Distro
        // lacks: the engine, `mode`, the inner product and L∞.
        let c = one(
            "knn_vector(3)",
            index(
                "items_emb",
                "emb",
                "cosine",
                &[("engine", "faiss"), ("m", "24"), ("mode", "on_disk")],
            ),
            "opendistro",
        );
        let t = &c.tables[0];
        assert_eq!(t.columns[1].data_type, "knn_vector(3)");
        assert_eq!(
            t.indexes[0].vector,
            Some(VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some("cosine".into()),
                params: [("m".to_string(), "24".to_string())].into(),
            })
        );
        for m in ["ip", "linf"] {
            let c = one(
                "knn_vector(3)",
                index("items_emb", "emb", m, &[]),
                "opendistro",
            );
            assert!(c.tables[0].indexes.is_empty(), "{m}");
            assert!(dropped(&c, &format!("«{m}»")), "{m}: {:?}", c.issues);
        }
        // Open Distro's distance and parameters belong to the whole index:
        // a second vector field searched otherwise loses its index.
        let mut t = table("knn_vector(3)", None);
        t.columns.push(dbine_driver::ColumnDef {
            name: "emb2".into(),
            data_type: "knn_vector(3)".into(),
            ..Default::default()
        });
        t.indexes = vec![
            index("items_emb", "emb", "cosine", &[]),
            index("items_emb2", "emb2", "l2", &[]),
        ];
        let c = crate::convert(
            &[t.clone()],
            "opensearch",
            "opendistro",
            &Default::default(),
        )
        .unwrap();
        assert_eq!(c.tables[0].indexes.len(), 1, "{:?}", c.tables[0].indexes);
        assert!(
            c.issues
                .iter()
                .any(|i| i.object.as_deref() == Some("items_emb2")
                    && i.severity == Severity::Dropped
                    && i.message.contains("«items_emb»")),
            "{:?}",
            c.issues
        );
        // OpenSearch keeps both, and its own settings as they are.
        let c = crate::convert(&[t], "opensearch", "opensearch", &Default::default()).unwrap();
        assert_eq!(c.tables[0].indexes.len(), 2);
        let c = one(
            "knn_vector(16, binary)",
            index("items_emb", "emb", "hamming", &[("engine", "faiss")]),
            "opensearch",
        );
        assert_eq!(c.tables[0].columns[1].data_type, "knn_vector(16, binary)");
        assert_eq!(
            c.tables[0].indexes[0]
                .vector
                .as_ref()
                .and_then(|v| v.params.get("engine"))
                .map(String::as_str),
            Some("faiss")
        );
        // Open Distro to Open Distro: its own settings map to themselves.
        let own = index(
            "items_emb",
            "emb",
            "l1",
            &[("m", "8"), ("ef_construction", "256")],
        );
        let mut t = table("knn_vector(3)", None);
        t.indexes = vec![own.clone()];
        let c = crate::convert(&[t], "opendistro", "opendistro", &Default::default()).unwrap();
        assert_eq!(c.tables[0].columns[1].data_type, "knn_vector(3)");
        assert_eq!(c.tables[0].indexes[0].vector, own.vector);
        assert!(
            !c.issues
                .iter()
                .any(|i| matches!(i.object.as_deref(), Some("emb" | "items_emb"))
                    && i.code != IssueCode::NullabilityChanged),
            "{:?}",
            c.issues
        );
    }

    #[test]
    fn opensearch_settings_stay_on_opensearch() {
        let mut t = table("knn_vector(3)", None);
        t.options.insert(
            "settings_extra".into(),
            r#"{"index.max_result_window":"50000","index.replication.type":"DOCUMENT","index.remote_store.enabled":"false"}"#.into(),
        );
        for dst in ["opendistro", "elasticsearch"] {
            let c = crate::convert(&[t.clone()], "opensearch", dst, &Default::default()).unwrap();
            assert_eq!(
                c.tables[0]
                    .options
                    .get("settings_extra")
                    .map(String::as_str),
                Some(r#"{"index.max_result_window":"50000"}"#),
                "{dst}"
            );
            assert_eq!(
                c.issues
                    .iter()
                    .filter(
                        |i| i.code == IssueCode::OptionDropped && i.message.contains("OpenSearch")
                    )
                    .count(),
                2,
                "{dst}: {:?}",
                c.issues
            );
        }
        let c = crate::convert(
            &[t.clone()],
            "opensearch",
            "opensearch",
            &Default::default(),
        )
        .unwrap();
        assert_eq!(c.tables[0].options, t.options);
        // Nothing else left: the option goes.
        t.options.insert(
            "settings_extra".into(),
            r#"{"index.replication.type":"DOCUMENT"}"#.into(),
        );
        let c = crate::convert(&[t], "opensearch", "opendistro", &Default::default()).unwrap();
        assert!(!c.tables[0].options.contains_key("settings_extra"));
    }

    #[test]
    fn index_names() {
        assert_eq!(index_name("Ventas"), "ventas");
        assert_eq!(index_name("_mis ventas"), "mis_ventas");
    }
}
