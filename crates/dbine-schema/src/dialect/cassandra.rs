//! Cassandra, ScyllaDB and Amazon Keyspaces (CQL).
//!
//! Types come from `system_schema.columns` as CQL spells them: scalars
//! (`text`, `bigint`, `timeuuid`…) and generics (`list<text>`,
//! `map<text, frozen<list<int>>>`, `tuple<int, text>`, `vector<float, 3>`).
//! Anything else is a user-defined type, which other engines get as JSON.
//!
//! The designer marks keys per column (`partition_key`, `clustering_key`,
//! `clustering_order`). From SQL, the key is deduced from the primary key
//! the way CQL itself reads `PRIMARY KEY (a, b, c)`: the first column is
//! the partition key and the others are clustering columns, ascending. So
//! the rows that share the key's first column (an order's lines, a
//! tenant's records) live together and come back sorted, and a lookup by
//! that column — the one SQL's leftmost-prefix rule makes cheap too —
//! stays a single-partition read. Without a primary key the first unique
//! index is the key, else every scalar column (rows that are equal in all
//! of them collapse into one; reported).
//!
//! CQL has no foreign keys, NOT NULL, defaults, auto-increment or unique
//! and multi-column indexes: all reported. Keyspaces has no secondary
//! indexes at all.
//!
//! Vectors are `vector<float, n>` (Cassandra 5, ScyllaDB): other engines'
//! vectors of halves, bytes or bits come as floats (their values fit
//! exactly), and a vector without a dimension, or a sparse one, can't be
//! one. Vector indexes are SAI on Cassandra and `vector_index` on
//! ScyllaDB, with cosine, Euclidean or dot product distance; only ScyllaDB takes
//! the graph settings from another engine (Cassandra needs a server flag
//! for them). Keyspaces has no vectors. A move among the three keeps the
//! source's types and indexes, so finishing the table checks each flavor's
//! limits again: ScyllaDB's 16000 dimensions, Cassandra's refusal of the
//! cosine distance on one dimension, and Keyspaces' lists for vectors.
//! ScyllaDB creates vector indexes only in keyspaces with tablets: said.

use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::vector::{metric, param};
use dbine_driver::{KeyDef, TableSchema, VectorElement, VectorIndex};
use std::collections::HashSet;

pub struct Cql {
    secondary_indexes: bool,
    /// `vector<float, n>` and vector indexes (not Keyspaces).
    vectors: bool,
    /// ScyllaDB: `vector_index`, with graph settings and filter columns.
    scylla: bool,
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static CQL: Cql = Cql {
        secondary_indexes: true,
        vectors: true,
        scylla: false,
    };
    static SCYLLA: Cql = Cql {
        secondary_indexes: true,
        vectors: true,
        scylla: true,
    };
    static KEYSPACES: Cql = Cql {
        secondary_indexes: false,
        vectors: false,
        scylla: false,
    };
    match driver_id {
        "cassandra" => Some(&CQL),
        "scylladb" => Some(&SCYLLA),
        "keyspaces" => Some(&KEYSPACES),
        _ => None,
    }
}

/// ScyllaDB's longest vector (`vector<float, 16000>`); Cassandra takes
/// longer ones.
const SCYLLA_MAX_DIMS: u32 = 16000;

/// What a vector of `n` dimensions becomes on ScyllaDB (`list`), said.
fn scylla_too_long(n: u32, list: &str) -> String {
    format!("ScyllaDB tiene vectores de hasta {SCYLLA_MAX_DIMS} dimensiones: uno de {n} queda como {list}.")
}

/// `frozen<inner>` → (`inner`, true); anything else as it is.
fn strip_frozen(s: &str) -> (&str, bool) {
    let t = s.trim();
    match t.get(..7) {
        Some(head) if head.eq_ignore_ascii_case("frozen<") && t.ends_with('>') => {
            (t[7..t.len() - 1].trim(), true)
        }
        _ => (t, false),
    }
}

/// A vector's element type as CQL spells it inside `vector<…, n>`: the
/// numbers an embedding is made of.
fn vector_element(cql: &str) -> Option<VectorElement> {
    match cql {
        "float" => Some(VectorElement::F32),
        "double" => Some(VectorElement::F64),
        "tinyint" => Some(VectorElement::I8),
        _ => None,
    }
}

/// ScyllaDB's `quantization` for another engine's (`int8`, `binary`,
/// `halfvec`…); `None` when there's no match.
fn scylla_quantization(q: &str) -> Option<&'static str> {
    Some(match q.trim().to_ascii_lowercase().as_str() {
        "f32" | "float32" | "fp32" | "none" | "noquant" => "f32",
        "f16" | "float16" | "fp16" | "half" | "halfvec" => "f16",
        "bf16" | "bfloat16" => "bf16",
        "i8" | "int8" | "q8" | "sq8" | "scalar" => "i8",
        "b1" | "bit" | "binary" | "bin" => "b1",
        _ => return None,
    })
}

/// `head<inner>` → (`head`, `inner`).
fn generic(s: &str) -> Option<(&str, &str)> {
    let open = s.find('<')?;
    let inner = s.strip_suffix('>')?;
    Some((s[..open].trim(), inner[open + 1..].trim()))
}

/// Split on commas outside `<…>`.
fn split_top(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

fn cql_type(s: &str) -> L {
    let s = s.trim().to_ascii_lowercase();
    if let Some((head, inner)) = generic(&s) {
        return match head {
            "frozen" => cql_type(inner),
            "list" | "set" => L::Array {
                of: Box::new(cql_type(inner)),
            },
            "map" => match split_top(inner).as_slice() {
                [k, v] => L::Map {
                    key: Box::new(cql_type(k)),
                    value: Box::new(cql_type(v)),
                },
                _ => L::Json { binary: true },
            },
            // `vector<float, 3>`: an embedding when it holds numbers of an
            // embedding's kind, otherwise a fixed-length list.
            "vector" => {
                let parts = split_top(inner);
                let elem = parts.first().copied().unwrap_or("float");
                let dims = parts.get(1).and_then(|d| d.trim().parse::<u32>().ok());
                match (vector_element(elem), dims) {
                    (Some(element), Some(dims)) => L::Vector {
                        dims: Some(dims),
                        element,
                        sparse: false,
                    },
                    _ => L::Array {
                        of: Box::new(cql_type(elem)),
                    },
                }
            }
            // Tuples and anything generic we don't know.
            _ => L::Json { binary: true },
        };
    }
    match s.as_str() {
        "boolean" => L::Bool,
        "tinyint" => L::int(1),
        "smallint" => L::int(2),
        "int" => L::int(4),
        "bigint" | "counter" => L::int(8),
        // Arbitrary precision integer.
        "varint" => L::Decimal {
            precision: None,
            scale: Some(0),
        },
        "decimal" => L::Decimal {
            precision: None,
            scale: None,
        },
        "float" => L::Float { bytes: 4 },
        "double" => L::Float { bytes: 8 },
        "text" | "varchar" => L::Text { unicode: true },
        "ascii" => L::Text { unicode: false },
        "blob" => L::Blob,
        "date" => L::Date,
        // Nanoseconds; SQL engines keep at most 6-7 digits.
        "time" => L::Time {
            precision: Some(9),
            tz: false,
        },
        // Milliseconds since the epoch, UTC.
        "timestamp" => L::Timestamp {
            precision: Some(3),
            tz: true,
        },
        "duration" => L::Interval,
        "uuid" | "timeuuid" => L::Uuid,
        "inet" => L::Inet,
        "" => L::Other {
            native: String::new(),
        },
        // A user-defined type: a structure, as JSON elsewhere.
        _ => L::Json { binary: true },
    }
}

fn is_collection(native: &str) -> bool {
    ["list<", "set<", "map<"]
        .iter()
        .any(|p| native.starts_with(p))
}

/// Collections nested in collections (or in keys) have to be frozen.
fn frozen(native: String) -> String {
    if is_collection(&native) {
        format!("frozen<{native}>")
    } else {
        native
    }
}

impl Cql {
    /// `vector<float, n>`, `vector<double, n>`; halves, bytes and bits as
    /// floats (their values fit exactly). Without a dimension, sparse, or
    /// on Keyspaces: a list, with the fallback's warning.
    fn render_vector(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let L::Vector {
            dims: Some(n),
            element,
            sparse: false,
        } = t
        else {
            return super::vector_fallback(self, t);
        };
        if !self.vectors {
            let list = super::vector_fallback(self, t).native;
            return Rendered::exact(list.clone()).with(
                Warning,
                TypeApproximated,
                format!(
                    "Amazon Keyspaces no tiene vectores: {} pasa a {list}, sin la dimensión.",
                    t.describe()
                ),
            );
        }
        if self.scylla && *n > SCYLLA_MAX_DIMS {
            return Rendered::exact("list<float>").with(
                Warning,
                TypeApproximated,
                scylla_too_long(*n, "list<float>"),
            );
        }
        let float = Rendered::exact(format!("vector<float, {n}>"));
        match element {
            VectorElement::F32 => float,
            VectorElement::F64 => Rendered::exact(format!("vector<double, {n}>")),
            VectorElement::F16 | VectorElement::Bf16 => float.with(
                Info,
                TypeChanged,
                "Vector de medios floats como vector<float>: CQL no los tiene, y los valores caben exactos.",
            ),
            VectorElement::I8 | VectorElement::U8 => float.with(
                Info,
                TypeChanged,
                "Vector de enteros de un byte como vector<float>: los índices vectoriales de CQL son de floats, y los valores caben exactos.",
            ),
            VectorElement::Bit => float.with(
                Warning,
                TypeApproximated,
                "Vector binario como vector<float> de 0 y 1: CQL no tiene vectores de bits ni distancia de Hamming.",
            ),
            VectorElement::Other => float.with(
                Warning,
                TypeApproximated,
                "Vector de números de tipo desconocido como vector<float>.",
            ),
        }
    }

    fn render(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Vector { .. } => self.render_vector(t),
            L::Bool => Rendered::exact("boolean"),
            L::Int { bytes, unsigned } => {
                let r = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 => Rendered::exact("tinyint"),
                    2 => Rendered::exact("smallint"),
                    3 | 4 => Rendered::exact("int"),
                    8 => Rendered::exact("bigint"),
                    _ => Rendered::exact("varint"),
                };
                if *unsigned {
                    r.with(
                        Info,
                        TypeChanged,
                        "CQL no tiene enteros sin signo: se usa un tipo más grande con signo.",
                    )
                } else {
                    r
                }
            }
            // Whole numbers of any size.
            L::Decimal { scale: Some(0), .. } => Rendered::exact("varint"),
            L::Decimal { .. } => Rendered::exact("decimal"),
            L::Float { bytes: 4 } => Rendered::exact("float"),
            L::Float { .. } => Rendered::exact("double"),
            L::Money => Rendered::exact("decimal").with(Info, TypeChanged, "Moneda como decimal."),
            L::Char { len: Some(_), .. } | L::Varchar { len: Some(_), .. } => {
                Rendered::exact("text").with(Info, TypeChanged, "CQL no limita el largo del texto.")
            }
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } => Rendered::exact("text"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("blob"),
            L::Bit { .. } => Rendered::exact("text").with(
                Warning,
                TypeApproximated,
                "Cadena de bits como texto de 0 y 1.",
            ),
            L::Date => Rendered::exact("date"),
            L::Time { tz, .. } => {
                let r = Rendered::exact("time");
                if *tz {
                    r.with(
                        Loss,
                        TimeZoneLoss,
                        "CQL no guarda la zona horaria de una hora.",
                    )
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact("timestamp");
                let r = match precision {
                    // The bulk copy refuses those values rather than truncate them.
                    Some(p) if *p > 3 => r.with(
                        Loss,
                        PrecisionLoss,
                        format!(
                            "timestamp de CQL guarda milisegundos y el origen tiene hasta {p} decimales de segundo: \
                             la copia de datos va a fallar con los valores que tengan dígitos más allá del milisegundo."
                        ),
                    ),
                    _ => r,
                };
                if *tz {
                    r
                } else {
                    r.with(Warning, TimeZoneLoss, "timestamp de CQL es un instante en UTC: la fecha y hora sin zona se toma como UTC.")
                }
            }
            L::Interval => Rendered::exact("duration"),
            L::Year => Rendered::exact("smallint").with(Info, TypeChanged, "Año como smallint."),
            L::Uuid => Rendered::exact("uuid"),
            L::Json { .. } => Rendered::exact("text").with(
                Warning,
                TypeApproximated,
                "CQL no tiene JSON: queda como texto.",
            ),
            L::Xml => Rendered::exact("text").with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } => Rendered::exact("text").with(
                Info,
                TypeApproximated,
                format!("Enumerado como texto. Valores: {}.", values.join(", ")),
            ),
            L::Set { values } => Rendered::exact("set<text>").with(
                Info,
                TypeApproximated,
                format!("Conjunto como set<text>. Valores: {}.", values.join(", ")),
            ),
            L::Array { of } => {
                let inner = self.render(of);
                Rendered {
                    native: format!("list<{}>", frozen(inner.native)),
                    notes: inner.notes,
                }
            }
            L::Map { key, value } => {
                let (k, v) = (self.render(key), self.render(value));
                let mut notes = k.notes;
                notes.extend(v.notes);
                Rendered {
                    native: format!("map<{}, {}>", frozen(k.native), frozen(v.native)),
                    notes,
                }
            }
            L::Geometry { .. } => Rendered::exact("text").with(
                Warning,
                TypeApproximated,
                "CQL no tiene tipos espaciales: queda como texto (WKT).",
            ),
            L::Inet => Rendered::exact("inet"),
            L::MacAddr => {
                Rendered::exact("text").with(Info, TypeApproximated, "Dirección MAC como texto.")
            }
            L::RowVersion => Rendered::exact("blob").with(
                Warning,
                TypeApproximated,
                "CQL no tiene versión de fila automática: no se actualiza sola.",
            ),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }
}

fn is_true(c: &dbine_driver::ColumnDef, k: &str) -> bool {
    c.options
        .get(k)
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// Settings of ScyllaDB's `vector_index` besides the distance (standard
/// names where there is one).
const SCYLLA_PARAMS: &[&str] = &[
    param::M,
    param::EF_CONSTRUCTION,
    param::QUANTIZATION,
    "search_beam_width",
    "oversampling",
    "rescoring",
];

impl Cql {
    /// Why this flavor can't index `column` as a vector with `metric`
    /// (none: the default, cosine): one without a dimension becomes a list
    /// and a sparse one text, which CQL doesn't index as vectors; ScyllaDB's
    /// vectors stop at 16000 dimensions; and Cassandra refuses the cosine
    /// distance on vectors of one dimension.
    fn vector_refusal(&self, column: Option<&L>, metric: Option<&str>) -> Option<String> {
        let metric = metric.map(str::trim).filter(|m| !m.is_empty());
        match column {
            Some(L::Vector { sparse: true, .. }) => {
                Some("su columna es un vector disperso, que en CQL queda como texto".into())
            }
            Some(L::Vector { dims: None, .. }) => Some(
                "su columna no dice su dimensión, así que en CQL queda como lista y no como vector<float, n>"
                    .into(),
            ),
            Some(L::Vector { dims: Some(n), .. }) if self.scylla && *n > SCYLLA_MAX_DIMS => {
                Some(format!(
                    "ScyllaDB tiene vectores de hasta {SCYLLA_MAX_DIMS} dimensiones, y su columna tiene {n}"
                ))
            }
            Some(L::Vector { dims: Some(1), .. })
                if !self.scylla && metric.is_none_or(|m| m.eq_ignore_ascii_case(metric::COSINE)) =>
            {
                Some("Cassandra no mide la distancia coseno en vectores de una dimensión".into())
            }
            _ => None,
        }
    }

    /// The vector indexes this flavor can't create on their column, left
    /// out with the reason. A move within CQL keeps the source's types and
    /// indexes as they are (the converter doesn't call
    /// [`Dialect::vector_index`] then), so the same checks run here: a
    /// Cassandra vector longer than ScyllaDB's, or a ScyllaDB cosine index
    /// on one dimension going to Cassandra.
    fn refuse_vector_indexes(&self, t: &mut TableSchema, report: &mut Report) {
        let types: std::collections::HashMap<&str, L> = t
            .columns
            .iter()
            .map(|c| (c.name.as_str(), cql_type(&c.data_type)))
            .collect();
        let mut kept = Vec::new();
        for ix in std::mem::take(&mut t.indexes) {
            let kind = ix.kind.as_deref().unwrap_or("");
            if ix.vector.is_none() && !kind.eq_ignore_ascii_case("vector_index") {
                kept.push(ix);
                continue;
            }
            let why = match ix.columns.first().and_then(|c| types.get(c.as_str())) {
                Some(col @ L::Vector { .. }) => self.vector_refusal(
                    Some(col),
                    ix.vector.as_ref().and_then(|v| v.metric.as_deref()),
                ),
                Some(_) => Some("su columna no es un vector".to_string()),
                None => None,
            };
            match why {
                Some(why) => report.push(
                    Severity::Dropped,
                    IssueCode::IndexDropped,
                    &t.name,
                    Some(&ix.name),
                    format!("El índice vectorial «{}» no se crea: {why}.", ix.name),
                ),
                None => kept.push(ix),
            }
        }
        t.indexes = kept;
    }

    /// Vector columns this flavor can't hold, as a list of their element,
    /// said: Keyspaces has no vectors, and ScyllaDB's stop at 16000
    /// dimensions. From another family the type is rendered so already;
    /// within CQL it comes as the source had it.
    fn vector_columns(&self, t: &mut TableSchema, report: &mut Report) {
        if self.vectors && !self.scylla {
            return;
        }
        let name = t.name.clone();
        for c in &mut t.columns {
            let (inner, was_frozen) = strip_frozen(&c.data_type);
            let Some((head, args)) = generic(inner) else {
                continue;
            };
            let parts = split_top(args);
            let (true, [elem, n]) = (head.eq_ignore_ascii_case("vector"), parts.as_slice()) else {
                continue;
            };
            let Ok(n) = n.parse::<u32>() else {
                continue;
            };
            if self.vectors && n <= SCYLLA_MAX_DIMS {
                continue;
            }
            let list = format!("list<{}>", frozen(elem.to_string()));
            let list = if was_frozen {
                format!("frozen<{list}>")
            } else {
                list
            };
            let message = if self.vectors {
                scylla_too_long(n, &list)
            } else {
                format!(
                    "Amazon Keyspaces no tiene vectores: {} pasa de {} a {list}, sin la dimensión.",
                    c.name, c.data_type
                )
            };
            report.push(
                Severity::Warning,
                IssueCode::TypeApproximated,
                &name,
                Some(&c.name),
                message,
            );
            c.data_type = list;
        }
    }

    /// Each vector index in this flavor's terms, every change reported.
    /// On Cassandra, an SAI index that takes only the distance (and
    /// DataStax's `source_model`): its graph settings (`m`,
    /// `ef_construction`) need the server option
    /// `cassandra.sai.vector.allow_custom_parameters`, which a default
    /// Cassandra refuses, and ScyllaDB's quantization, beam widths, filter
    /// columns and local (partition) indexes don't exist there. An SAI
    /// index read from Cassandra itself (`same_family`, kind `sai`) keeps
    /// what it has: its server had the option. On ScyllaDB, a
    /// `vector_index` with the settings it has (DataStax's options and
    /// other engines' are left out). The source's algorithm (`hnsw`,
    /// `ivfflat`…) becomes CQL's graph. And the vector column becomes
    /// `vector<float, n>`: CQL indexes vectors of floats only.
    fn vector_indexes(&self, t: &mut TableSchema, report: &mut Report, same_family: bool) {
        if !self.vectors {
            return;
        }
        let name = t.name.clone();
        let mut float_columns: Vec<String> = Vec::new();
        for ix in &mut t.indexes {
            let kind = ix.kind.as_deref().unwrap_or("").to_ascii_lowercase();
            let scylla_kind = kind == "vector_index";
            if ix.vector.is_none() && !scylla_kind {
                continue;
            }
            // Kept as read: an index of this flavor from this flavor.
            let own = same_family && kind == if self.scylla { "vector_index" } else { "sai" };
            let v = ix.vector.get_or_insert_with(VectorIndex::default);
            let mut dropped: Vec<String> = Vec::new();
            let mut graph: Vec<String> = Vec::new();
            if let Some(a) = v
                .algorithm
                .take()
                .filter(|a| !own && !a.eq_ignore_ascii_case("hnsw"))
            {
                report.push(
                    Severity::Info,
                    IssueCode::IndexChanged,
                    &name,
                    Some(&ix.name),
                    format!(
                        "El índice vectorial «{}» ({a}) pasa a ser el índice de grafo de CQL.",
                        ix.name
                    ),
                );
            }
            if !own {
                let keys: Vec<String> = v.params.keys().cloned().collect();
                for k in keys {
                    let val = v.params[&k].clone();
                    let keep = if self.scylla {
                        match k.as_str() {
                            param::QUANTIZATION => match scylla_quantization(&val) {
                                Some(q) => {
                                    v.params.insert(k.clone(), q.to_string());
                                    true
                                }
                                None => false,
                            },
                            param::M | param::EF_CONSTRUCTION => val.trim().parse::<u32>().is_ok(),
                            other => SCYLLA_PARAMS.contains(&other),
                        }
                    } else {
                        k == "source_model"
                    };
                    if keep {
                        continue;
                    }
                    v.params.remove(&k);
                    if !self.scylla && (k == param::M || k == param::EF_CONSTRUCTION) {
                        graph.push(format!("{k}={val}"));
                    } else {
                        dropped.push(format!("{k}={val}"));
                    }
                }
            }
            if !graph.is_empty() {
                report.push(
                    Severity::Info,
                    IssueCode::OptionDropped,
                    &name,
                    Some(&ix.name),
                    format!(
                        "El índice vectorial «{}» se crea con el grafo por defecto de Cassandra: sus parámetros ({}) piden la opción cassandra.sai.vector.allow_custom_parameters en el servidor.",
                        ix.name,
                        graph.join(", ")
                    ),
                );
            }
            if !dropped.is_empty() {
                let (ix_name, list) = (&ix.name, dropped.join(", "));
                report.push(
                    Severity::Info,
                    IssueCode::OptionDropped,
                    &name,
                    Some(&ix.name),
                    if self.scylla {
                        format!("El índice vectorial «{ix_name}» no lleva {list}: ScyllaDB no los tiene.")
                    } else {
                        format!("El índice vectorial «{ix_name}» no lleva {list}: Cassandra no los tiene.")
                    },
                );
            }
            if !own {
                // ScyllaDB's local index (its partition columns) only on
                // ScyllaDB; any other option is the source engine's.
                if !self.scylla && ix.options.remove("pk").is_some() {
                    report.push(
                        Severity::Warning,
                        IssueCode::IndexChanged,
                        &name,
                        Some(&ix.name),
                        format!(
                            "El índice vectorial «{}» era local a cada partición: en Cassandra es de toda la tabla.",
                            ix.name
                        ),
                    );
                }
                let others: Vec<String> = ix
                    .options
                    .iter()
                    .filter(|(k, _)| !(self.scylla && k.as_str() == "pk"))
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                if !others.is_empty() {
                    ix.options.retain(|k, _| self.scylla && k == "pk");
                    report.push(
                        Severity::Info,
                        IssueCode::OptionDropped,
                        &name,
                        Some(&ix.name),
                        format!(
                            "se quitan las opciones del índice «{}»: {}",
                            ix.name,
                            others.join(", ")
                        ),
                    );
                }
            }
            // Filter columns after the vector column only on ScyllaDB;
            // Cassandra's SAI indexes the vector alone.
            if !self.scylla && ix.columns.len() > 1 {
                report.push(
                    Severity::Warning,
                    IssueCode::IndexChanged,
                    &name,
                    Some(&ix.name),
                    format!(
                        "El índice vectorial de Cassandra (SAI) no tiene columnas de filtro: indexa solo {} (quedan fuera {}).",
                        ix.columns[0],
                        ix.columns[1..].join(", ")
                    ),
                );
                ix.columns.truncate(1);
            }
            ix.kind = Some(if self.scylla { "vector_index" } else { "sai" }.to_string());
            if self.scylla {
                report.push(
                    Severity::Info,
                    IssueCode::IndexChanged,
                    &name,
                    Some(&ix.name),
                    format!(
                        "El índice vectorial «{}» pide un keyspace con tablets: ScyllaDB no crea índices vectoriales en uno sin ellos, y los tablets de un keyspace no se cambian después de crearlo.",
                        ix.name
                    ),
                );
            }
            if let Some(c) = ix.columns.first() {
                float_columns.push(c.clone());
            }
        }
        // The indexed vectors as floats.
        for c in &mut t.columns {
            if !float_columns.contains(&c.name) {
                continue;
            }
            let ty = c.data_type.trim().to_ascii_lowercase();
            let inner = ty
                .strip_prefix("frozen<")
                .and_then(|r| r.strip_suffix('>'))
                .unwrap_or(&ty);
            let Some((head, args)) = generic(inner) else {
                continue;
            };
            let parts = split_top(args);
            if head != "vector" || parts.len() != 2 || parts[0] == "float" {
                continue;
            }
            let float = format!("vector<float, {}>", parts[1]);
            report.push(
                Severity::Warning,
                IssueCode::PrecisionLoss,
                &name,
                Some(&c.name),
                format!(
                    "{} pasa de {} a {float}: los índices vectoriales de CQL son de vectores de floats.",
                    c.name, c.data_type
                ),
            );
            c.data_type = float;
        }
    }
}

impl Dialect for Cql {
    fn id(&self) -> &'static str {
        "cassandra"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        match cql_type(&t.raw) {
            L::Other { .. } => L::Other {
                native: t.raw.clone(),
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
            foreign_keys: false,
            on_delete: &[],
            on_update: &[],
            indexes: self.secondary_indexes,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: false,
            nullability: false,
            comments: false,
            max_identifier: 48,
            case: IdentCase::Lower,
            vector_indexes: self.vectors,
            vectors: self.vectors,
        }
    }

    /// SAI on Cassandra, `vector_index` on ScyllaDB, with the distances
    /// CQL has. The settings go on as they came (with the source's
    /// algorithm): [`Dialect::finalize`] keeps those the flavor takes and
    /// reports the rest (see `Cql::vector_indexes`). The column has to
    /// become a `vector<…, n>`: one without a dimension becomes a list and
    /// a sparse one text, which CQL doesn't index as vectors. And Cassandra
    /// refuses the cosine distance (its default) on vectors of one
    /// dimension.
    fn vector_index(
        &self,
        v: &VectorIndex,
        ctx: &super::VectorIndexCtx<'_>,
    ) -> Result<super::VectorIndexMapping, String> {
        let column = ctx.column;
        if !self.vectors {
            return Err("Amazon Keyspaces no tiene índices vectoriales".into());
        }
        if let Some(why) = self.vector_refusal(column, v.metric.as_deref()) {
            return Err(why);
        }
        let metric = match v.metric.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            None => None,
            Some(m) => Some(
                [
                    (metric::COSINE, "cosine"),
                    (metric::L2, "euclidean"),
                    (metric::IP, "dot_product"),
                ]
                .iter()
                .find(|(std, cql)| m.eq_ignore_ascii_case(std) || m.eq_ignore_ascii_case(cql))
                .map(|(std, _)| std.to_string())
                .ok_or_else(|| {
                    format!(
                        "CQL no tiene la distancia «{m}»: solo coseno, euclídea y producto punto"
                    )
                })?,
            ),
        };
        let kind = if self.scylla { "vector_index" } else { "sai" };
        Ok(super::VectorIndexMapping::new(
            Some(kind.to_string()),
            VectorIndex {
                algorithm: v.algorithm.clone(),
                metric,
                params: v.params.clone(),
            },
        ))
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        // Read from a CQL server: the key is already in the column options.
        let same_family = t.columns.iter().any(|c| is_true(c, "partition_key"));
        if self.vectors {
            self.refuse_vector_indexes(t, report);
        }
        self.vector_indexes(t, report, same_family);
        self.vector_columns(t, report);
        if same_family {
            return;
        }
        let name = t.name.clone();
        t.kind = dbine_driver::kinds::TABLE.into();
        let exists = |c: &String| t.columns.iter().any(|x| &x.name == c);
        let mut key: Vec<String> = t
            .primary_key
            .as_ref()
            .map(|k| k.columns.clone())
            .unwrap_or_default();
        if key.is_empty() {
            if let Some(ix) = t
                .indexes
                .iter()
                .find(|i| i.unique && !i.columns.is_empty() && i.columns.iter().all(exists))
            {
                key = ix.columns.clone();
                report.push(
                    Severity::Warning,
                    IssueCode::PrimaryKeyAdded,
                    &name,
                    Some(&ix.name),
                    format!(
                        "La tabla no tiene clave primaria: se usa la del índice único «{}».",
                        ix.name
                    ),
                );
            } else {
                key = t
                    .columns
                    .iter()
                    .filter(|c| !is_collection(&c.data_type))
                    .map(|c| c.name.clone())
                    .collect();
                report.push(
                    Severity::Warning,
                    IssueCode::PrimaryKeyAdded,
                    &name,
                    None,
                    "La tabla no tiene clave primaria y CQL la exige: la forman todas las columnas simples, así que las filas iguales en todas ellas quedan en una sola.",
                );
            }
        }
        for (i, k) in key.iter().enumerate() {
            let Some(c) = t.columns.iter_mut().find(|c| &c.name == k) else {
                continue;
            };
            if i == 0 {
                c.options.insert("partition_key".into(), "true".into());
            } else {
                c.options.insert("clustering_key".into(), "true".into());
                c.options.insert("clustering_order".into(), "ASC".into());
            }
            if is_collection(&c.data_type) {
                c.data_type = frozen(std::mem::take(&mut c.data_type));
            }
        }
        if let Some((first, rest)) = key.split_first() {
            let msg = if rest.is_empty() {
                format!("Clave de partición: {first}.")
            } else {
                format!(
                    "Clave de partición: {first}; columnas de clustering (ascendentes): {}. Las filas con el mismo {first} quedan juntas en una partición: si son muchas, conviene revisar la clave.",
                    rest.join(", ")
                )
            };
            report.push(
                Severity::Info,
                IssueCode::OptionAdded,
                &name,
                Some("partition_key"),
                msg,
            );
        }
        t.primary_key = (!key.is_empty()).then(|| KeyDef {
            name: t.primary_key.as_ref().and_then(|k| k.name.clone()),
            columns: key.clone(),
        });

        // Secondary indexes: one column, not unique, not the sole partition key.
        let mut indexed: HashSet<String> = HashSet::new();
        let mut kept = Vec::new();
        for ix in std::mem::take(&mut t.indexes) {
            let why = if ix.unique {
                Some("CQL no tiene índices únicos: la unicidad no se controla.")
            } else if ix.columns.len() != 1 && !(self.scylla && ix.vector.is_some()) {
                Some("Un índice secundario de CQL lleva una sola columna.")
            } else if key.len() == 1 && key[0] == ix.columns[0] {
                Some("La columna ya es la clave de partición.")
            } else if !indexed.insert(ix.columns[0].clone()) {
                Some("Ya hay un índice sobre esa columna.")
            } else {
                None
            };
            match why {
                Some(m) => report.push(
                    Severity::Dropped,
                    IssueCode::IndexDropped,
                    &name,
                    Some(&ix.name),
                    m,
                ),
                None => kept.push(ix),
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

    fn p(s: &str) -> L {
        lookup("cassandra").unwrap().parse_type(&parse(s))
    }

    #[test]
    fn parses_cql_types() {
        assert_eq!(p("text"), L::Text { unicode: true });
        assert_eq!(p("ascii"), L::Text { unicode: false });
        assert_eq!(p("bigint"), L::int(8));
        assert_eq!(p("counter"), L::int(8));
        assert_eq!(
            p("varint"),
            L::Decimal {
                precision: None,
                scale: Some(0)
            }
        );
        assert_eq!(
            p("decimal"),
            L::Decimal {
                precision: None,
                scale: None
            }
        );
        assert_eq!(
            p("timestamp"),
            L::Timestamp {
                precision: Some(3),
                tz: true
            }
        );
        assert_eq!(
            p("time"),
            L::Time {
                precision: Some(9),
                tz: false
            }
        );
        assert_eq!(p("timeuuid"), L::Uuid);
        assert_eq!(p("duration"), L::Interval);
        assert_eq!(p("inet"), L::Inet);
        assert_eq!(
            p("list<text>"),
            L::Array {
                of: Box::new(L::Text { unicode: true })
            }
        );
        assert_eq!(
            p("set<int>"),
            L::Array {
                of: Box::new(L::int(4))
            }
        );
        assert_eq!(
            p("map<text, frozen<list<int>>>"),
            L::Map {
                key: Box::new(L::Text { unicode: true }),
                value: Box::new(L::Array {
                    of: Box::new(L::int(4))
                })
            }
        );
        assert_eq!(p("frozen<tuple<int, text>>"), L::Json { binary: true });
        assert_eq!(
            p("vector<float, 3>"),
            L::Vector {
                dims: Some(3),
                element: VectorElement::F32,
                sparse: false
            }
        );
        assert_eq!(p("direccion"), L::Json { binary: true });
        assert_eq!(p("frozen<direccion>"), L::Json { binary: true });
    }

    #[test]
    fn renders_every_variant() {
        let d = lookup("cassandra").unwrap();
        let r = |t: L| d.render_type(&t).native;
        assert_eq!(r(L::Bool), "boolean");
        assert_eq!(r(L::int(1)), "tinyint");
        assert_eq!(
            r(L::Int {
                bytes: 4,
                unsigned: true
            }),
            "bigint"
        );
        assert_eq!(
            r(L::Int {
                bytes: 8,
                unsigned: true
            }),
            "varint"
        );
        assert_eq!(
            r(L::Decimal {
                precision: Some(10),
                scale: Some(2)
            }),
            "decimal"
        );
        assert_eq!(
            r(L::Decimal {
                precision: Some(20),
                scale: Some(0)
            }),
            "varint"
        );
        assert_eq!(r(L::Float { bytes: 4 }), "float");
        assert_eq!(r(L::Float { bytes: 8 }), "double");
        assert_eq!(r(L::Money), "decimal");
        assert_eq!(
            r(L::Varchar {
                len: Some(3),
                unicode: false
            }),
            "text"
        );
        assert_eq!(r(L::Blob), "blob");
        assert_eq!(r(L::Bit { len: Some(1) }), "text");
        assert_eq!(r(L::Date), "date");
        assert_eq!(
            r(L::Time {
                precision: Some(6),
                tz: false
            }),
            "time"
        );
        assert_eq!(
            r(L::Timestamp {
                precision: Some(6),
                tz: true
            }),
            "timestamp"
        );
        assert_eq!(r(L::Interval), "duration");
        assert_eq!(r(L::Year), "smallint");
        assert_eq!(r(L::Uuid), "uuid");
        assert_eq!(r(L::Json { binary: true }), "text");
        assert_eq!(r(L::Xml), "text");
        assert_eq!(r(L::Enum { values: vec![] }), "text");
        assert_eq!(r(L::Set { values: vec![] }), "set<text>");
        assert_eq!(
            r(L::Array {
                of: Box::new(L::Array {
                    of: Box::new(L::int(4))
                })
            }),
            "list<frozen<list<int>>>"
        );
        assert_eq!(
            r(L::Map {
                key: Box::new(L::Text { unicode: true }),
                value: Box::new(L::int(8))
            }),
            "map<text, bigint>"
        );
        assert_eq!(
            r(L::Geometry {
                kind: None,
                srid: None,
                geography: false
            }),
            "text"
        );
        assert_eq!(r(L::Inet), "inet");
        assert_eq!(r(L::MacAddr), "text");
        assert_eq!(r(L::RowVersion), "blob");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
        assert!(!lookup("keyspaces").unwrap().caps().indexes);
    }

    /// Sub-millisecond digits make the data copy fail (it never truncates
    /// them): the note says so, at the severity of values that don't fit.
    #[test]
    fn finer_timestamps_warn_that_the_copy_fails() {
        let d = lookup("cassandra").unwrap();
        let r = d.render_type(&L::Timestamp {
            precision: Some(6),
            tz: true,
        });
        assert_eq!(r.native, "timestamp");
        let [n] = r.notes.as_slice() else {
            panic!("{:?}", r.notes)
        };
        assert_eq!(
            (n.severity, n.code),
            (Severity::Loss, IssueCode::PrecisionLoss)
        );
        assert!(
            n.message.contains("6 decimales") && n.message.contains("va a fallar"),
            "{}",
            n.message
        );
        assert!(d
            .render_type(&L::Timestamp {
                precision: Some(3),
                tz: true
            })
            .notes
            .is_empty());
        assert!(d
            .render_type(&L::Timestamp {
                precision: None,
                tz: true
            })
            .notes
            .is_empty());
    }

    fn vector(dims: Option<u32>, element: VectorElement) -> L {
        L::Vector {
            dims,
            element,
            sparse: false,
        }
    }

    #[test]
    fn vectors_parse_and_render_natively() {
        assert_eq!(p("vector<double, 2>"), vector(Some(2), VectorElement::F64));
        assert_eq!(
            p("frozen<vector<float, 1536>>"),
            vector(Some(1536), VectorElement::F32)
        );
        assert_eq!(p("vector<tinyint, 4>"), vector(Some(4), VectorElement::I8));
        // Not an embedding: a fixed-length list.
        assert_eq!(
            p("vector<text, 2>"),
            L::Array {
                of: Box::new(L::Text { unicode: true })
            }
        );
        assert_eq!(
            p("vector<int, 2>"),
            L::Array {
                of: Box::new(L::int(4))
            }
        );
        for id in ["cassandra", "scylladb"] {
            let d = lookup(id).unwrap();
            for native in ["vector<float, 3>", "vector<double, 2>"] {
                let r = d.render_type(&p(native));
                assert_eq!(r.native, native);
                assert!(r.notes.is_empty(), "{native}: {:?}", r.notes);
            }
            // Other engines' halves, bytes and bits: floats, said so.
            for e in [
                VectorElement::F16,
                VectorElement::Bf16,
                VectorElement::I8,
                VectorElement::U8,
            ] {
                let r = d.render_type(&vector(Some(8), e));
                assert_eq!(r.native, "vector<float, 8>");
                assert_eq!(r.notes[0].severity, Severity::Info);
            }
            // ScyllaDB's longest vector is 16000; Cassandra takes more.
            let r = d.render_type(&vector(Some(20000), VectorElement::F32));
            if id == "scylladb" {
                assert_eq!(r.native, "list<float>");
                assert!(r.notes[0].message.contains("16000"), "{r:?}");
            } else {
                assert_eq!(r.native, "vector<float, 20000>");
            }
            let r = d.render_type(&vector(Some(16), VectorElement::Bit));
            assert_eq!(r.native, "vector<float, 16>");
            assert_eq!(r.notes[0].severity, Severity::Warning);
            // No dimension, or sparse: a vector<…> can't hold it.
            let r = d.render_type(&vector(None, VectorElement::F32));
            assert_eq!(r.native, "list<float>");
            assert_eq!(r.notes[0].code, IssueCode::TypeApproximated);
            let r = d.render_type(&L::Vector {
                dims: Some(5),
                element: VectorElement::F32,
                sparse: true,
            });
            assert_eq!(r.native, "text");
        }
        // Keyspaces has no vectors.
        let k = lookup("keyspaces").unwrap();
        assert_eq!(
            k.render_type(&vector(Some(3), VectorElement::F32)).native,
            "list<float>"
        );
    }

    /// pgvector's `vector(3)` is `Vector { dims: 3, f32 }`: it comes as
    /// `vector<float, 3>`, and goes back as that same logical type.
    #[test]
    fn pgvector_round_trip() {
        let pg = vector(Some(3), VectorElement::F32);
        let d = lookup("cassandra").unwrap();
        let native = d.render_type(&pg).native;
        assert_eq!(native, "vector<float, 3>");
        assert_eq!(p(&native), pg);
    }

    fn params(kv: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// A table as convert hands it to finalize: an `id` key, a vector
    /// column of `ty` and an index on it.
    fn vector_table(ty: &str, ix: dbine_driver::IndexDef, same_family: bool) -> TableSchema {
        let col = |n: &str, t: &str, key: bool| {
            let mut c = dbine_driver::ColumnDef {
                name: n.into(),
                data_type: t.into(),
                ..Default::default()
            };
            if key && same_family {
                c.options.insert("partition_key".into(), "true".into());
            }
            c
        };
        TableSchema {
            name: "docs".into(),
            columns: vec![
                col("id", "int", true),
                col("emb", ty, false),
                col("lang", "text", false),
            ],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into()],
            }),
            indexes: vec![ix],
            ..Default::default()
        }
    }

    fn finalized(id: &str, mut t: TableSchema) -> (TableSchema, Report) {
        let mut r = Report::default();
        lookup(id).unwrap().finalize(&mut t, &mut r);
        (t, r)
    }

    fn says(r: &Report, code: IssueCode, words: &[&str]) -> bool {
        r.issues
            .iter()
            .any(|i| i.code == code && words.iter().all(|w| i.message.contains(w)))
    }

    #[test]
    fn vector_indexes_from_other_engines() {
        let hnsw = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("cosine".into()),
            params: params(&[("m", "16"), ("ef_construction", "64"), ("lists", "100")]),
        };
        // The distance is mapped; the settings go on to finalize.
        let (kind, v) = lookup("cassandra")
            .unwrap()
            .vector_index_pair(&hnsw, None)
            .unwrap();
        assert_eq!(kind.as_deref(), Some("sai"));
        assert_eq!(v.metric.as_deref(), Some("cosine"));
        let ix = |kind: &str, v: VectorIndex| dbine_driver::IndexDef {
            name: "docs_emb".into(),
            columns: vec!["emb".into()],
            kind: Some(kind.into()),
            vector: Some(v),
            ..Default::default()
        };
        // Cassandra: SAI with the distance alone, each setting left out
        // said (the graph's, with the server option it needs).
        let (t, r) = finalized(
            "cassandra",
            vector_table("vector<float, 3>", ix("sai", v), false),
        );
        let got = t.indexes[0].vector.as_ref().unwrap();
        assert_eq!(
            got,
            &VectorIndex {
                metric: Some("cosine".into()),
                ..Default::default()
            }
        );
        assert!(says(
            &r,
            IssueCode::OptionDropped,
            &["m=16", "ef_construction=64", "allow_custom_parameters"]
        ));
        assert!(says(
            &r,
            IssueCode::OptionDropped,
            &["lists=100", "Cassandra no los tiene"]
        ));
        // ScyllaDB: vector_index with the graph settings it has.
        let (kind, v) = lookup("scylladb")
            .unwrap()
            .vector_index_pair(&hnsw, None)
            .unwrap();
        assert_eq!(kind.as_deref(), Some("vector_index"));
        let (t, r) = finalized(
            "scylladb",
            vector_table("vector<float, 3>", ix("vector_index", v), false),
        );
        let got = t.indexes[0].vector.as_ref().unwrap();
        assert_eq!(got.algorithm, None);
        assert_eq!(
            got.params,
            params(&[("ef_construction", "64"), ("m", "16")])
        );
        assert!(says(
            &r,
            IssueCode::OptionDropped,
            &["lists=100", "ScyllaDB no los tiene"]
        ));
        // Quantization by ScyllaDB's name; an IVF index is the graph.
        let q = VectorIndex {
            algorithm: Some("ivfflat".into()),
            metric: Some("l2".into()),
            params: params(&[("quantization", "int8")]),
        };
        let (_, v) = lookup("scylladb")
            .unwrap()
            .vector_index_pair(&q, None)
            .unwrap();
        let (t, r) = finalized(
            "scylladb",
            vector_table("vector<float, 3>", ix("vector_index", v), false),
        );
        let got = t.indexes[0].vector.as_ref().unwrap();
        assert_eq!(got.metric.as_deref(), Some("l2"));
        assert_eq!(got.params["quantization"], "i8");
        assert!(says(&r, IssueCode::IndexChanged, &["ivfflat"]));
        // Distances CQL lacks, and Keyspaces.
        let l1 = VectorIndex {
            metric: Some("l1".into()),
            ..Default::default()
        };
        assert!(lookup("cassandra")
            .unwrap()
            .vector_index_pair(&l1, None)
            .unwrap_err()
            .contains("l1"));
        assert!(lookup("keyspaces")
            .unwrap()
            .vector_index_pair(&hnsw, None)
            .is_err());
    }

    /// Within CQL: a ScyllaDB vector_index to Cassandra is an SAI index
    /// with the distance alone (no graph settings a default Cassandra
    /// refuses, no local or filter columns); a Cassandra SAI index to
    /// ScyllaDB a vector_index without DataStax's options; an SAI index
    /// read from Cassandra keeps its graph settings.
    #[test]
    fn vector_indexes_within_cql() {
        let scylla_ix = dbine_driver::IndexDef {
            name: "docs_emb".into(),
            columns: vec!["emb".into(), "lang".into()],
            kind: Some("vector_index".into()),
            vector: Some(VectorIndex {
                metric: Some("l2".into()),
                params: params(&[
                    ("m", "24"),
                    ("quantization", "i8"),
                    ("search_beam_width", "64"),
                ]),
                ..Default::default()
            }),
            options: params(&[("pk", "id")]),
            ..Default::default()
        };
        let (t, r) = finalized(
            "cassandra",
            vector_table("vector<float, 3>", scylla_ix.clone(), true),
        );
        let ix = &t.indexes[0];
        assert_eq!(ix.kind.as_deref(), Some("sai"));
        assert_eq!(ix.columns, ["emb"]);
        assert!(ix.options.is_empty());
        assert_eq!(
            ix.vector.as_ref().unwrap(),
            &VectorIndex {
                metric: Some("l2".into()),
                ..Default::default()
            }
        );
        assert!(says(
            &r,
            IssueCode::OptionDropped,
            &["m=24", "allow_custom_parameters"]
        ));
        assert!(says(
            &r,
            IssueCode::OptionDropped,
            &["quantization=i8", "search_beam_width=64"]
        ));
        assert!(says(&r, IssueCode::IndexChanged, &["local"]));
        assert!(says(&r, IssueCode::IndexChanged, &["lang"]));
        // ScyllaDB to ScyllaDB: as it is.
        let (t, r) = finalized(
            "scylladb",
            vector_table("vector<float, 3>", scylla_ix, true),
        );
        assert_eq!(t.indexes[0].vector.as_ref().unwrap().params.len(), 3);
        assert_eq!(t.indexes[0].options["pk"], "id");
        // Only that ScyllaDB's vector indexes need a keyspace with tablets.
        assert_eq!(r.issues.len(), 1, "{:?}", r.issues);
        assert!(says(&r, IssueCode::IndexChanged, &["docs_emb", "tablets"]));
        // Cassandra's SAI with graph settings and a source model.
        let sai = dbine_driver::IndexDef {
            name: "docs_emb".into(),
            columns: vec!["emb".into()],
            kind: Some("sai".into()),
            vector: Some(VectorIndex {
                metric: Some("cosine".into()),
                params: params(&[("m", "32"), ("source_model", "OPENAI_V3_SMALL")]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (t, r) = finalized(
            "cassandra",
            vector_table("vector<float, 3>", sai.clone(), true),
        );
        assert_eq!(t.indexes[0].vector.as_ref().unwrap().params.len(), 2);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        let (t, r) = finalized("scylladb", vector_table("vector<float, 3>", sai, true));
        let ix = &t.indexes[0];
        assert_eq!(ix.kind.as_deref(), Some("vector_index"));
        assert_eq!(ix.vector.as_ref().unwrap().params, params(&[("m", "32")]));
        assert!(says(
            &r,
            IssueCode::OptionDropped,
            &["source_model=OPENAI_V3_SMALL"]
        ));
    }

    /// CQL indexes vectors of floats: an indexed `vector<double, n>` (an
    /// Oracle FLOAT64 vector, say) becomes `vector<float, n>`, said; one
    /// without an index stays.
    #[test]
    fn indexed_vectors_are_floats() {
        for id in ["cassandra", "scylladb"] {
            let ix = dbine_driver::IndexDef {
                name: "docs_emb".into(),
                columns: vec!["emb".into()],
                kind: Some(
                    if id == "cassandra" {
                        "sai"
                    } else {
                        "vector_index"
                    }
                    .into(),
                ),
                vector: Some(VectorIndex::default()),
                ..Default::default()
            };
            let (t, r) = finalized(id, vector_table("vector<double, 3>", ix, false));
            assert_eq!(t.columns[1].data_type, "vector<float, 3>", "{id}");
            assert!(
                says(&r, IssueCode::PrecisionLoss, &["vector<double, 3>"]),
                "{id}"
            );
            let mut plain = vector_table("vector<double, 3>", Default::default(), false);
            plain.indexes.clear();
            let (t, _) = finalized(id, plain);
            assert_eq!(t.columns[1].data_type, "vector<double, 3>");
        }
    }

    #[test]
    fn vector_index_filter_columns_only_on_scylla() {
        let table = || TableSchema {
            name: "docs".into(),
            columns: vec![
                dbine_driver::ColumnDef {
                    name: "id".into(),
                    data_type: "int".into(),
                    ..Default::default()
                },
                dbine_driver::ColumnDef {
                    name: "emb".into(),
                    data_type: "vector<float, 3>".into(),
                    ..Default::default()
                },
                dbine_driver::ColumnDef {
                    name: "lang".into(),
                    data_type: "text".into(),
                    ..Default::default()
                },
            ],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into()],
            }),
            indexes: vec![dbine_driver::IndexDef {
                name: "docs_emb".into(),
                columns: vec!["emb".into(), "lang".into()],
                vector: Some(VectorIndex::default()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut t = table();
        let mut r = Report::default();
        lookup("scylladb").unwrap().finalize(&mut t, &mut r);
        assert_eq!(t.indexes[0].columns, ["emb", "lang"]);
        let mut t = table();
        let mut r = Report::default();
        lookup("cassandra").unwrap().finalize(&mut t, &mut r);
        assert_eq!(t.indexes[0].columns, ["emb"]);
        assert!(r
            .issues
            .iter()
            .any(|i| i.code == IssueCode::IndexChanged && i.message.contains("lang")));
    }

    /// The column a vector index searches decides whether CQL can index
    /// it: a vector without a dimension (a list in CQL) or a sparse one
    /// (text) can't, and Cassandra measures no cosine on one dimension.
    #[test]
    fn vector_indexes_look_at_their_column() {
        let cosine = VectorIndex {
            metric: Some("cosine".into()),
            ..Default::default()
        };
        let l2 = VectorIndex {
            metric: Some("l2".into()),
            ..Default::default()
        };
        let col = |dims: Option<u32>, sparse: bool| L::Vector {
            dims,
            element: VectorElement::F32,
            sparse,
        };
        for id in ["cassandra", "scylladb"] {
            let d = lookup(id).unwrap();
            assert!(d
                .vector_index_pair(&cosine, Some(&col(Some(3), false)))
                .is_ok());
            let e = d
                .vector_index_pair(&cosine, Some(&col(None, false)))
                .unwrap_err();
            assert!(e.contains("dimensión"), "{e}");
            let e = d
                .vector_index_pair(&cosine, Some(&col(Some(30), true)))
                .unwrap_err();
            assert!(e.contains("disperso"), "{e}");
        }
        // ScyllaDB's vectors have up to 16000 dimensions; Cassandra's more.
        let long = col(Some(20000), false);
        let e = lookup("scylladb")
            .unwrap()
            .vector_index_pair(&cosine, Some(&long))
            .unwrap_err();
        assert!(e.contains("16000"), "{e}");
        assert!(lookup("scylladb")
            .unwrap()
            .vector_index_pair(&cosine, Some(&col(Some(16000), false)))
            .is_ok());
        let c = lookup("cassandra").unwrap();
        assert!(c.vector_index_pair(&cosine, Some(&long)).is_ok());
        let one = col(Some(1), false);
        assert!(c.vector_index_pair(&cosine, Some(&one)).is_err());
        assert!(c
            .vector_index_pair(&VectorIndex::default(), Some(&one))
            .is_err());
        assert!(c.vector_index_pair(&l2, Some(&one)).is_ok());
        // ScyllaDB measures cosine on one dimension too.
        assert!(lookup("scylladb")
            .unwrap()
            .vector_index_pair(&cosine, Some(&one))
            .is_ok());
        assert!(!lookup("keyspaces").unwrap().caps().vector_indexes);
        assert!(c.caps().vector_indexes);
    }

    /// Whole conversions into CQL: a searched float array with a stated
    /// dimension arrives as a vector with its index; a list without one,
    /// a vector without a dimension or a sparse one leaves the index out.
    #[test]
    fn vector_indexes_converted_into_cql() {
        use crate::convert::{convert, Options};
        use dbine_driver::{ColumnDef, IndexDef};
        let source = |id_type: &str, ty: &str, kind: &str, v: VectorIndex| TableSchema {
            name: "docs".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: id_type.into(),
                    ..Default::default()
                },
                ColumnDef {
                    name: "emb".into(),
                    data_type: ty.into(),
                    ..Default::default()
                },
            ],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into()],
            }),
            indexes: vec![IndexDef {
                name: "docs_emb".into(),
                columns: vec!["emb".into()],
                kind: Some(kind.into()),
                vector: Some(v),
                ..Default::default()
            }],
            ..Default::default()
        };
        let one = |t: TableSchema, from: &str, to: &str| {
            let r = convert(&[t], from, to, &Options::default()).unwrap();
            (r.tables[0].clone(), r.issues)
        };
        let dropped = |issues: &[crate::issue::Issue], word: &str| {
            issues
                .iter()
                .any(|i| i.code == IssueCode::IndexDropped && i.message.contains(word))
        };
        // ClickHouse: Array(Float32) searched by an index of 3 dimensions.
        let ch = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("l2".into()),
            params: params(&[("dimensions", "3")]),
        };
        for to in ["cassandra", "scylladb"] {
            let (t, _) = one(
                source("Int32", "Array(Float32)", "vector_similarity", ch.clone()),
                "clickhouse",
                to,
            );
            assert_eq!(t.columns[1].data_type, "vector<float, 3>", "{to}");
            let ix = &t.indexes[0];
            assert_eq!(
                ix.kind.as_deref(),
                Some(if to == "scylladb" {
                    "vector_index"
                } else {
                    "sai"
                })
            );
            assert_eq!(ix.vector.as_ref().unwrap().metric.as_deref(), Some("l2"));
        }
        // Without a stated dimension ClickHouse's array is a vector of no
        // dimension: a list in CQL, which no vector index takes.
        let mut no_dims = ch.clone();
        no_dims.params.clear();
        let (t, issues) = one(
            source("Int32", "Array(Float32)", "vector_similarity", no_dims),
            "clickhouse",
            "cassandra",
        );
        assert!(
            t.indexes.is_empty() && dropped(&issues, "dimensión"),
            "{issues:?}"
        );
        // A column that isn't a vector at all: the converter says so.
        let (t, issues) = one(
            source("Int32", "String", "vector_similarity", ch.clone()),
            "clickhouse",
            "cassandra",
        );
        assert!(
            t.indexes.is_empty() && dropped(&issues, "no es un vector"),
            "{issues:?}"
        );
        // pgvector: a vector of no dimension, a sparse one, one dimension.
        let hnsw = |metric: &str| VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some(metric.into()),
            ..Default::default()
        };
        let (t, issues) = one(
            source("integer", "vector", "hnsw", hnsw("cosine")),
            "postgres",
            "cassandra",
        );
        assert!(
            t.indexes.is_empty() && dropped(&issues, "dimensión"),
            "{issues:?}"
        );
        let (t, issues) = one(
            source("integer", "sparsevec(30)", "hnsw", hnsw("ip")),
            "postgres",
            "scylladb",
        );
        assert!(
            t.indexes.is_empty() && dropped(&issues, "disperso"),
            "{issues:?}"
        );
        let (t, issues) = one(
            source("integer", "vector(1)", "hnsw", hnsw("cosine")),
            "postgres",
            "cassandra",
        );
        assert_eq!(t.columns[1].data_type, "vector<float, 1>");
        assert!(
            t.indexes.is_empty() && dropped(&issues, "coseno"),
            "{issues:?}"
        );
        let (t, _) = one(
            source("integer", "vector(1)", "hnsw", hnsw("l2")),
            "postgres",
            "cassandra",
        );
        assert_eq!(t.indexes.len(), 1);
        // Keyspaces: no vector indexes (nor any secondary index).
        let (t, _) = one(
            source("integer", "vector(3)", "hnsw", hnsw("cosine")),
            "postgres",
            "keyspaces",
        );
        assert!(t.indexes.is_empty());
    }

    /// Cassandra, ScyllaDB and Keyspaces share the dialect, so a move among
    /// them keeps the source's types and indexes as they are: each flavor's
    /// limits are checked when the table is finished. A Cassandra vector
    /// longer than ScyllaDB's becomes a list there and its index is left
    /// out; a ScyllaDB cosine index on one dimension is left out on
    /// Cassandra; and Keyspaces gets a list for any vector.
    #[test]
    fn vector_moves_within_cql_keep_to_each_flavor() {
        use crate::convert::{convert, Options};
        use crate::issue::Issue;
        use dbine_driver::IndexDef;
        let source = |ty: &str, kind: &str, metric: Option<&str>| {
            vector_table(
                ty,
                IndexDef {
                    name: "docs_emb".into(),
                    columns: vec!["emb".into()],
                    kind: Some(kind.into()),
                    vector: Some(VectorIndex {
                        metric: metric.map(Into::into),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                true,
            )
        };
        let one = |t: TableSchema, from: &str, to: &str| {
            let r = convert(&[t], from, to, &Options::default()).unwrap();
            (r.tables[0].clone(), r.issues)
        };
        let has = |issues: &[Issue], code: IssueCode, words: &[&str]| {
            issues
                .iter()
                .any(|i| i.code == code && words.iter().all(|w| i.message.contains(w)))
        };
        // Cassandra's 20000 dimensions, with an SAI index, into ScyllaDB.
        let (t, issues) = one(
            source("vector<float, 20000>", "sai", Some("cosine")),
            "cassandra",
            "scylladb",
        );
        assert_eq!(t.columns[1].data_type, "list<float>");
        assert!(t.indexes.is_empty(), "{:?}", t.indexes);
        assert!(
            has(
                &issues,
                IssueCode::IndexDropped,
                &["docs_emb", "16000", "20000"]
            ),
            "{issues:?}"
        );
        assert!(
            has(
                &issues,
                IssueCode::TypeApproximated,
                &["16000", "list<float>"]
            ),
            "{issues:?}"
        );
        // ScyllaDB's longest is kept, index and all.
        let (t, _) = one(
            source("vector<float, 16000>", "sai", Some("cosine")),
            "cassandra",
            "scylladb",
        );
        assert_eq!(t.columns[1].data_type, "vector<float, 16000>");
        assert_eq!(t.indexes[0].kind.as_deref(), Some("vector_index"));
        // Into Cassandra, the same vector stays as it is.
        let (t, issues) = one(
            source("vector<float, 20000>", "sai", Some("cosine")),
            "cassandra",
            "cassandra",
        );
        assert_eq!(t.columns[1].data_type, "vector<float, 20000>");
        assert_eq!(t.indexes.len(), 1, "{issues:?}");
        // ScyllaDB's cosine index on one dimension (the default distance
        // too) into Cassandra, which refuses it; Euclidean goes.
        for metric in [Some("cosine"), None] {
            let (t, issues) = one(
                source("vector<float, 1>", "vector_index", metric),
                "scylladb",
                "cassandra",
            );
            assert_eq!(t.columns[1].data_type, "vector<float, 1>");
            assert!(t.indexes.is_empty(), "{metric:?}: {:?}", t.indexes);
            assert!(
                has(&issues, IssueCode::IndexDropped, &["docs_emb", "coseno"]),
                "{issues:?}"
            );
        }
        let (t, _) = one(
            source("vector<float, 1>", "vector_index", Some("l2")),
            "scylladb",
            "cassandra",
        );
        assert_eq!(t.indexes[0].kind.as_deref(), Some("sai"));
        let (t, _) = one(
            source("vector<float, 1>", "vector_index", Some("cosine")),
            "scylladb",
            "scylladb",
        );
        assert_eq!(t.indexes.len(), 1);
        // Keyspaces has no vectors: a list, said (by the conversion, which
        // takes the family's list for a vector within the family too), and
        // no index.
        let (t, issues) = one(
            source("vector<float, 3>", "sai", Some("cosine")),
            "cassandra",
            "keyspaces",
        );
        assert_eq!(t.columns[1].data_type, "list<float>");
        assert!(t.indexes.is_empty());
        assert!(
            issues.iter().any(|i| i.code == IssueCode::TypeApproximated
                && i.object.as_deref() == Some("emb")
                && i.message.contains("Keyspaces")
                && i.message.contains("list<float>")),
            "{issues:?}"
        );
        // A frozen vector in a key stays frozen as a list.
        let mut t = source("frozen<vector<double, 20000>>", "sai", None);
        t.indexes.clear();
        let (t, _) = finalized("scylladb", t);
        assert_eq!(t.columns[1].data_type, "frozen<list<double>>");
    }
}
