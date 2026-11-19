//! MongoDB, FerretDB and Amazon DocumentDB.
//!
//! A collection has no declared columns: `database_schema` reports the
//! fields of a sample with the BSON types seen, most frequent first and
//! joined with `|` (`string|int`), and `_id` as the primary key. The
//! designer turns the fields back into a `$jsonSchema` validator: the BSON
//! type (`bsonType`), `null` admitted when the field is nullable and the
//! column option `required` for fields every document must have.
//!
//! From SQL:
//! - every column gets its BSON type and NOT NULL becomes `required` plus a
//!   `bsonType` without `null`;
//! - the primary key stays as ordinary fields with a unique index
//!   (`<table>_pk`), and MongoDB gives each document its own `_id`. Mapping
//!   the key onto `_id` would rename a column the data copy fills by name,
//!   wouldn't work for composite keys and would leave foreign key columns
//!   of other tables pointing at a field that no longer exists. A key
//!   that already is `_id` (a collection that went to SQL and comes back)
//!   stays `_id`;
//! - foreign keys are dropped (reported), defaults too (MongoDB has none).
//!
//! To SQL: nested objects and arrays become JSON, `ObjectId` a 24-character
//! hex string (how the driver shows it), dates a timestamp with time zone
//! in milliseconds (BSON dates are UTC instants), a field seen with more
//! than one type JSON (numeric mixes widen to the largest number).

use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{IndexDef, TableSchema};

pub struct Mongo {
    /// `$jsonSchema` validators are enforced (not by FerretDB).
    validators: bool,
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static MONGO: Mongo = Mongo { validators: true };
    static FERRET: Mongo = Mongo { validators: false };
    match driver_id {
        "mongodb" | "documentdb" => Some(&MONGO),
        "ferretdb" => Some(&FERRET),
        _ => None,
    }
}

/// The types of an inferred field: `string|int`, `integer | string`,
/// lower case, without `null` (nullability is its own flag).
pub(crate) fn type_list(raw: &str) -> Vec<String> {
    raw.split('|')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty() && t != "null")
        .collect()
}

/// One logical type for a field seen with several: numbers widen to the
/// largest, anything else mixed is JSON (a document field can hold any
/// value, and a JSON column is the SQL column that can too).
pub(crate) fn unify(types: Vec<L>) -> L {
    let mut it = types.into_iter();
    let Some(first) = it.next() else {
        return L::Text { unicode: true };
    };
    it.fold(first, |a, b| match (a, b) {
        (a, b) if a == b => a,
        (L::Int { bytes: x, .. }, L::Int { bytes: y, .. }) => L::int(x.max(y)),
        (L::Decimal { .. }, n) | (n, L::Decimal { .. }) if is_number(&n) => L::Decimal {
            precision: None,
            scale: None,
        },
        (a, b) if is_number(&a) && is_number(&b) => L::Float { bytes: 8 },
        _ => L::Json { binary: true },
    })
}

fn is_number(t: &L) -> bool {
    matches!(t, L::Int { .. } | L::Float { .. } | L::Decimal { .. })
}

/// Parse `raw` as a `|` list with `one` classifying each type.
pub(crate) fn parse_list(raw: &str, one: impl Fn(&str) -> L) -> L {
    let types = type_list(raw);
    if types.is_empty() {
        // Only nulls seen (or no type): text is the least surprising.
        return L::Text { unicode: true };
    }
    unify(types.iter().map(|t| one(t)).collect())
}

fn bson(t: &str) -> L {
    match t {
        "double" | "number" => L::Float { bytes: 8 },
        "int" | "integer" | "int32" => L::int(4),
        "long" | "int64" => L::int(8),
        "decimal" | "decimal128" => L::Decimal {
            precision: None,
            scale: None,
        },
        "bool" | "boolean" => L::Bool,
        "string" | "symbol" | "javascript" | "regex" | "minkey" | "maxkey" | "dbpointer"
        | "undefined" => L::Text { unicode: true },
        // An internal (time, increment) pair, shown as `Timestamp(t, i)`.
        "timestamp" => L::Text { unicode: true },
        "objectid" | "oid" => L::Char {
            len: Some(24),
            unicode: false,
        },
        "date" | "datetime" => L::Timestamp {
            precision: Some(3),
            tz: true,
        },
        "bindata" | "binary" => L::Blob,
        "object" | "array" | "any" | "mixed" | "*" => L::Json { binary: true },
        _ => L::Other {
            native: t.to_string(),
        },
    }
}

/// `vector`, `vector(N)`, `vector(N, float32 | int8 | bit)` (the driver's
/// spelling of a vector field: an array of numbers or a BinData vector).
pub(crate) fn vector_type(t: &str) -> Option<L> {
    use dbine_driver::VectorElement as E;
    let t = t.trim().to_ascii_lowercase();
    let rest = t.strip_prefix("vector")?.trim();
    let mut dims = None;
    let mut element = E::F32;
    if !rest.is_empty() {
        let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
        for part in inner.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if let Ok(n) = part.parse::<u32>() {
                dims = Some(n);
            } else {
                element = match part {
                    "float32" | "float" | "double" | "number" => E::F32,
                    "int8" => E::I8,
                    "uint8" => E::U8,
                    "bit" | "int1" | "packed_bit" => E::Bit,
                    _ => return None,
                };
            }
        }
    }
    Some(L::Vector {
        dims,
        element,
        sparse: false,
    })
}

/// A vector index in the MongoDB family's terms: `cosine`, `l2`
/// (`euclidean`) and `ip` (`dotProduct`); the algorithm (Atlas has one,
/// DocumentDB `hnsw` / `ivfflat`, FerretDB `hnsw` / `ivf`) and the kind
/// are the driver's choice for the server it writes to.
pub(crate) fn vector_index(
    v: &dbine_driver::VectorIndex,
    family: &str,
) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
    use dbine_driver::vector::{metric, param};
    let mut out = dbine_driver::VectorIndex::default();
    if let Some(m) = v.metric.as_deref() {
        out.metric = Some(match m.to_ascii_lowercase().as_str() {
            metric::COSINE | metric::L2 | metric::IP => m.to_ascii_lowercase(),
            // The same nearest neighbors as l2: the driver writes l2 and
            // says so (`vector::for_flavor`).
            "l2_squared" | "euclidean_squared" => "l2_squared".into(),
            _ => {
                return Err(format!(
                    "{family} no tiene la distancia «{m}» (cosine, l2 o ip)"
                ))
            }
        });
    }
    out.algorithm = v
        .algorithm
        .as_deref()
        .map(str::to_ascii_lowercase)
        .filter(|a| matches!(a.as_str(), "hnsw" | "ivfflat" | "ivf" | "diskann"));
    out.params = v
        .params
        .iter()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                param::M | param::EF_CONSTRUCTION | param::LISTS | param::QUANTIZATION
            )
        })
        .map(|(k, x)| (k.clone(), x.clone()))
        .collect();
    Ok((None, out))
}

/// A dense vector as a vector field (`vector(N)`, with `int8` / `bit`);
/// other element types are numbers in an array, exactly.
pub(crate) fn render_vector(t: &L) -> Option<Rendered> {
    use dbine_driver::VectorElement as E;
    let L::Vector {
        dims,
        element,
        sparse: false,
    } = t
    else {
        return None;
    };
    let el = match element {
        E::I8 => ", int8",
        E::Bit => ", bit",
        _ => "",
    };
    Some(Rendered::exact(match dims {
        Some(n) => format!("vector({n}{el})"),
        None if el.is_empty() => "vector".into(),
        None => format!("vector({})", &el[2..]),
    }))
}

impl Dialect for Mongo {
    fn id(&self) -> &'static str {
        "mongodb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let raw = t.raw.trim();
        if raw.is_empty() {
            return L::Json { binary: true };
        }
        // A vector field (`vector(1536)`, `vector(3, int8)`), alone or with
        // the forms it's stored in (`array|vector(3)`).
        let types = type_list(raw);
        if let Some(v) = types.iter().find_map(|t| vector_type(t)) {
            if types
                .iter()
                .all(|t| vector_type(t).is_some() || t == "array" || t == "bindata")
            {
                return v;
            }
        }
        match parse_list(raw, bson) {
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
            L::Vector { .. } => render_vector(t).unwrap_or_else(|| super::vector_fallback(self, t)),
            L::Bool => Rendered::exact("bool"),
            L::Int { bytes, unsigned } => match L::signed_bytes_for(*bytes, *unsigned) {
                1..=4 => Rendered::exact("int"),
                8 => Rendered::exact("long"),
                _ => Rendered::exact("decimal").with(Info, TypeChanged, "Entero de más de 8 bytes como decimal (Decimal128)."),
            },
            L::Decimal { precision: Some(p), .. } if *p <= 34 => Rendered::exact("decimal"),
            L::Decimal { precision, .. } => Rendered::exact("decimal").with(
                Loss,
                PrecisionLoss,
                match precision {
                    Some(p) => format!("Decimal128 guarda hasta 34 dígitos significativos; el origen admite {p}."),
                    None => "Decimal128 guarda hasta 34 dígitos significativos; el origen no fija la precisión.".into(),
                },
            ),
            L::Float { .. } => Rendered::exact("double"),
            L::Money => Rendered::exact("decimal").with(Info, TypeChanged, "Moneda como decimal (Decimal128)."),
            L::Char { len: Some(_), .. } | L::Varchar { len: Some(_), .. } => {
                Rendered::exact("string").with(Info, TypeChanged, "MongoDB no limita el largo del texto.")
            }
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } => Rendered::exact("string"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("binData"),
            L::Bit { .. } => Rendered::exact("string").with(Warning, TypeApproximated, "Cadena de bits como texto de 0 y 1."),
            L::Date => Rendered::exact("date").with(Info, TypeChanged, "Fecha como date de BSON, a las 00:00 UTC."),
            L::Time { .. } => Rendered::exact("string").with(Warning, TypeApproximated, "BSON no tiene hora del día: queda como texto."),
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact("date");
                let r = match precision {
                    Some(p) if *p > 3 => r.with(Loss, PrecisionLoss, format!("BSON guarda milisegundos: se pierden {} decimales de segundo.", p - 3)),
                    _ => r,
                };
                if *tz {
                    r
                } else {
                    r.with(Warning, TimeZoneLoss, "Las fechas de BSON son instantes en UTC: la fecha y hora sin zona se toma como UTC.")
                }
            }
            L::Interval => Rendered::exact("string").with(Warning, TypeApproximated, "BSON no tiene intervalos: queda como texto."),
            L::Year => Rendered::exact("int").with(Info, TypeChanged, "Año como entero."),
            L::Uuid => Rendered::exact("string").with(Info, TypeChanged, "UUID como texto de 36 caracteres."),
            L::Json { .. } => Rendered::exact("object|array").with(Info, TypeChanged, "JSON como documento o arreglo anidado."),
            L::Xml => Rendered::exact("string").with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } => Rendered::exact("string").with(Info, TypeApproximated, format!("Enumerado como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => Rendered::exact("array").with(Info, TypeApproximated, format!("Conjunto como arreglo. Valores: {}.", values.join(", "))),
            L::Array { of } => Rendered::exact("array").with(Info, TypeChanged, format!("El validador no controla el tipo de los elementos ({}).", of.describe())),
            L::Map { .. } => Rendered::exact("object"),
            L::Geometry { .. } => Rendered::exact("object").with(Warning, TypeApproximated, "Dato espacial como documento: MongoDB usa GeoJSON, hay que convertir los valores."),
            L::Inet | L::MacAddr => Rendered::exact("string").with(Info, TypeApproximated, "Dirección como texto."),
            L::RowVersion => Rendered::exact("binData").with(Warning, TypeApproximated, "MongoDB no tiene versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, _d: &DefaultValue, _ty: &L) -> Option<String> {
        None
    }

    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
        _column: Option<&crate::logical::LogicalType>,
    ) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
        vector_index(v, "MongoDB")
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: false,
            on_delete: &[],
            on_update: &[],
            indexes: true,
            // `partialFilterExpression` is a query document, not SQL.
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: false,
            nullability: self.validators,
            comments: true,
            max_identifier: 255,
            case: IdentCase::Lower,
            vector_indexes: true,
        }
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        // A collection read from a MongoDB-family server: already native.
        if ["validate_fields", "validator", "viewOn"]
            .iter()
            .any(|k| t.options.contains_key(*k))
        {
            return;
        }
        t.kind = dbine_driver::kinds::COLLECTION.into();
        if !self.validators {
            t.options.insert("validate_fields".into(), "false".into());
            report.push(Severity::Info, IssueCode::OptionAdded, &t.name, Some("validate_fields"), "FerretDB no aplica validadores $jsonSchema: la colección se crea sin validar los campos.");
        }
        let name = t.name.clone();
        if self.validators {
            for c in t.columns.iter_mut().filter(|c| !c.nullable) {
                c.options.insert("required".into(), "true".into());
            }
        }
        if let Some(pk) = t.primary_key.take() {
            if pk.columns == ["_id"] {
                // Back from SQL: the hex text the driver showed is an ObjectId
                // again when the copy writes it (and text otherwise).
                if let Some(c) = t
                    .columns
                    .iter_mut()
                    .find(|c| c.name == "_id" && c.data_type == "string")
                {
                    c.data_type = "objectId|string".into();
                }
                t.primary_key = Some(pk);
            } else if !pk.columns.is_empty() {
                let ix = format!("{name}_pk");
                report.push(
                    Severity::Info,
                    IssueCode::PrimaryKeyDropped,
                    &name,
                    Some(&ix),
                    format!(
                        "La clave primaria ({}) queda como índice único «{ix}»; MongoDB le agrega a cada documento su propio _id.",
                        pk.columns.join(", ")
                    ),
                );
                if !t
                    .indexes
                    .iter()
                    .any(|i| i.unique && i.columns == pk.columns)
                {
                    t.indexes.insert(
                        0,
                        IndexDef {
                            name: ix,
                            columns: pk.columns,
                            unique: true,
                            ..Default::default()
                        },
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn p(s: &str) -> L {
        lookup("mongodb").unwrap().parse_type(&parse(s))
    }

    #[test]
    fn parses_inferred_types() {
        assert_eq!(p("string"), L::Text { unicode: true });
        assert_eq!(
            p("objectId"),
            L::Char {
                len: Some(24),
                unicode: false
            }
        );
        assert_eq!(p("int"), L::int(4));
        assert_eq!(p("long"), L::int(8));
        assert_eq!(p("int|long"), L::int(8));
        assert_eq!(p("double|int"), L::Float { bytes: 8 });
        assert_eq!(
            p("decimal|int"),
            L::Decimal {
                precision: None,
                scale: None
            }
        );
        assert_eq!(p("string|int"), L::Json { binary: true });
        assert_eq!(
            p("date"),
            L::Timestamp {
                precision: Some(3),
                tz: true
            }
        );
        assert_eq!(p("bool"), L::Bool);
        assert_eq!(p("object"), L::Json { binary: true });
        assert_eq!(p("array"), L::Json { binary: true });
        assert_eq!(p("binData"), L::Blob);
        assert_eq!(p("null"), L::Text { unicode: true });
        assert_eq!(p("timestamp"), L::Text { unicode: true });
        assert_eq!(p(""), L::Json { binary: true });
        assert_eq!(p("integer | boolean"), L::Json { binary: true });
        assert!(matches!(p("weird"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let d = lookup("mongodb").unwrap();
        let r = |t: L| d.render_type(&t).native;
        assert_eq!(r(L::Bool), "bool");
        assert_eq!(r(L::int(2)), "int");
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
            "decimal"
        );
        assert_eq!(
            r(L::Decimal {
                precision: Some(12),
                scale: Some(2)
            }),
            "decimal"
        );
        assert!(!d
            .render_type(&L::Decimal {
                precision: Some(40),
                scale: Some(2)
            })
            .notes
            .is_empty());
        assert_eq!(r(L::Float { bytes: 4 }), "double");
        assert_eq!(r(L::Money), "decimal");
        assert_eq!(
            r(L::Varchar {
                len: Some(20),
                unicode: true
            }),
            "string"
        );
        assert_eq!(r(L::Text { unicode: false }), "string");
        assert_eq!(r(L::Blob), "binData");
        assert_eq!(r(L::Bit { len: Some(3) }), "string");
        assert_eq!(r(L::Date), "date");
        assert_eq!(
            r(L::Time {
                precision: None,
                tz: false
            }),
            "string"
        );
        let ts = d.render_type(&L::Timestamp {
            precision: Some(6),
            tz: false,
        });
        assert_eq!(ts.native, "date");
        assert!(ts.notes.iter().any(|n| n.code == IssueCode::PrecisionLoss));
        assert!(ts.notes.iter().any(|n| n.code == IssueCode::TimeZoneLoss));
        assert_eq!(r(L::Interval), "string");
        assert_eq!(r(L::Year), "int");
        assert_eq!(r(L::Uuid), "string");
        assert_eq!(r(L::Json { binary: true }), "object|array");
        assert_eq!(r(L::Xml), "string");
        assert_eq!(
            r(L::Enum {
                values: vec!["a".into()]
            }),
            "string"
        );
        assert_eq!(
            r(L::Set {
                values: vec!["a".into()]
            }),
            "array"
        );
        assert_eq!(
            r(L::Array {
                of: Box::new(L::int(4))
            }),
            "array"
        );
        assert_eq!(
            r(L::Map {
                key: Box::new(L::Text { unicode: true }),
                value: Box::new(L::int(4))
            }),
            "object"
        );
        assert_eq!(
            r(L::Geometry {
                kind: None,
                srid: None,
                geography: false
            }),
            "object"
        );
        assert_eq!(r(L::Inet), "string");
        assert_eq!(r(L::MacAddr), "string");
        assert_eq!(r(L::RowVersion), "binData");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
        assert_eq!(
            d.render_default(&DefaultValue::CurrentTimestamp, &L::Date),
            None
        );
    }

    #[test]
    fn vector_fields_round_trip() {
        use dbine_driver::VectorElement as E;
        let d = lookup("mongodb").unwrap();
        let v = |dims, element| L::Vector {
            dims,
            element,
            sparse: false,
        };
        assert_eq!(p("vector(1536)"), v(Some(1536), E::F32));
        assert_eq!(p("vector(3, int8)"), v(Some(3), E::I8));
        assert_eq!(p("vector(9, bit)"), v(Some(9), E::Bit));
        assert_eq!(p("array|vector(3)|null"), v(Some(3), E::F32));
        assert_eq!(p("vector"), v(None, E::F32));
        // Mixed with other types it's the JSON it always was.
        assert_eq!(p("string|vector(3)"), L::Json { binary: true });
        for (t, native) in [
            (v(Some(3), E::F32), "vector(3)"),
            (v(Some(3), E::F64), "vector(3)"),
            (v(Some(3), E::I8), "vector(3, int8)"),
            (v(Some(8), E::Bit), "vector(8, bit)"),
            (v(None, E::F32), "vector"),
        ] {
            let r = d.render_type(&t);
            assert_eq!(r.native, native);
            assert!(r.notes.is_empty(), "{native}");
            assert_eq!(
                p(&r.native),
                match t {
                    L::Vector {
                        dims,
                        element: E::F64,
                        ..
                    } => v(dims, E::F32),
                    other => other,
                }
            );
        }
        // Sparse vectors have no MongoDB form: the fallback with a note.
        let sparse = d.render_type(&L::Vector {
            dims: Some(5),
            element: E::F32,
            sparse: true,
        });
        assert!(!sparse.notes.is_empty());
    }

    #[test]
    fn pgvector_columns_and_indexes_become_mongodb_ones() {
        use dbine_driver::{VectorElement as E, VectorIndex};
        // pgvector's `vector(3)` reads as this logical type (the PostgreSQL
        // dialect's side), and a MongoDB field gives it back.
        let mongo = lookup("mongodb").unwrap();
        let lt = L::Vector {
            dims: Some(3),
            element: E::F32,
            sparse: false,
        };
        assert_eq!(mongo.render_type(&lt).native, "vector(3)");
        assert_eq!(p("vector(3)"), lt);
        let hnsw = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("cosine".into()),
            params: [("m", "16"), ("ef_construction", "64"), ("opclass", "x")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let (kind, v) = mongo.vector_index(&hnsw, None).unwrap();
        assert_eq!(kind, None);
        assert_eq!(v.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(v.params.len(), 2);
        let l1 = VectorIndex {
            metric: Some("l1".into()),
            ..Default::default()
        };
        assert!(mongo.vector_index(&l1, None).is_err());
        // Couchbase's squared euclidean goes on for the driver to write as
        // l2 (with a warning).
        let sq = VectorIndex {
            metric: Some("L2_SQUARED".into()),
            ..Default::default()
        };
        assert_eq!(
            mongo.vector_index(&sq, None).unwrap().1.metric.as_deref(),
            Some("l2_squared")
        );
    }

    #[test]
    fn ferretdb_skips_validators() {
        let f = lookup("ferretdb").unwrap();
        assert!(!f.caps().nullability);
        assert!(lookup("mongodb").unwrap().caps().nullability);
        assert!(lookup("documentdb").is_some() && lookup("redis").is_none());
    }
}
