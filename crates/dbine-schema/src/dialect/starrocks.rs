//! StarRocks, Apache Doris and VeloDB (managed Doris). MySQL protocol and
//! MySQL-like names, but an OLAP storage model: every table has a key
//! model (PRIMARY / UNIQUE / DUPLICATE KEY) whose columns go first and a
//! hash distribution; no foreign keys, no unique indexes, VARCHAR lengths
//! in bytes.
//!
//! Also home of the helpers the other analytical dialects share: parsing
//! `ARRAY<…>` / `MAP<…>` spellings and nested element types.

use super::postgres::{longest, precision_loss};
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::{quote, DefaultValue};
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{self, TypeSpec};
use dbine_driver::TableSchema;

pub struct Olap {
    doris: bool,
}

/// Longest VARCHAR, in bytes.
const SR_MAX_VARCHAR: u32 = 1_048_576;
const DORIS_MAX_VARCHAR: u32 = 65_533;
/// Bytes per character when a character length becomes a byte length (utf8mb4).
const UTF8_BYTES: u32 = 4;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static SR: Olap = Olap { doris: false };
    static DORIS: Olap = Olap { doris: true };
    match driver_id {
        "starrocks" => Some(&SR),
        "doris" | "velodb" => Some(&DORIS),
        _ => None,
    }
}

/// `array<int(11)>` → (`array`, [`int(11)`]); `struct<a int, b varchar(5)>`
/// → (`struct`, [`a int`, `b varchar(5)`]). `None` without angle brackets.
pub(crate) fn angle(raw: &str) -> Option<(String, Vec<String>)> {
    let raw = raw.trim();
    let open = raw.find('<')?;
    if !raw.ends_with('>') {
        return None;
    }
    let name = raw[..open].trim().to_ascii_lowercase();
    let inner = &raw[open + 1..raw.len() - 1];
    let mut out = Vec::new();
    let (mut depth, mut cur) = (0i32, String::new());
    for c in inner.chars() {
        match c {
            '<' | '(' => depth += 1,
            '>' | ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    Some((name, out))
}

/// A vector's dimension rides in the rendered type as `/* dim=N */` (or
/// `/* dim=? */` when the source type didn't say it) until
/// [`Dialect::finalize`] settles it with [`settle_vectors`]: into the
/// type, or into the column's vector index where the engine keeps it there.
pub(crate) fn with_dim(native: &str, dims: Option<u32>) -> String {
    match dims {
        Some(n) => format!("{native} /* dim={n} */"),
        None => format!("{native} /* dim=? */"),
    }
}

/// The type without its dimension mark, and the mark: `Some(Some(n))` for
/// `/* dim=N */`, `Some(None)` for `/* dim=? */`, `None` without one.
pub(crate) fn take_dim(ty: &str) -> (String, Option<Option<u32>>) {
    match ty.find(" /* dim=") {
        Some(at) => {
            let n = ty[at + 8..].split(' ').next().and_then(|n| n.parse().ok());
            (ty[..at].to_string(), Some(n))
        }
        None => (ty.to_string(), None),
    }
}

/// Index parameters that hold a vector's dimension: Doris and StarRocks'
/// `dim`, Manticore's `knn_dims`. A target's `vector_index` keeps them, so
/// that [`settle_vectors`] can give the indexed column its dimension.
pub(crate) const DIM_PARAMS: &[&str] = &["dim", "knn_dims"];

/// The dimension a vector index holds, if it does.
pub(crate) fn index_dim(v: &dbine_driver::VectorIndex) -> Option<u32> {
    DIM_PARAMS
        .iter()
        .find_map(|k| v.params.get(*k))
        .and_then(|n| n.trim().parse().ok())
        .filter(|n| *n > 0)
}

/// The dimension of the vectors an index searches, as the source says it:
/// its column's type (`column`, the logical type `convert` hands to
/// [`Dialect::vector_index`]), else the index's own (Doris' `dim`,
/// Manticore's `knn_dims`).
pub(crate) fn searched_dims(v: &dbine_driver::VectorIndex, column: Option<&L>) -> Option<u32> {
    match column {
        Some(L::Vector { dims: Some(n), .. }) if *n > 0 => Some(*n),
        _ => index_dim(v),
    }
}

/// Why `engine`'s vector indexes can't take an index on `column`: a sparse
/// vector (none of this crate's engines index one), or a vector whose
/// dimension nothing says when the engine needs it (`needs_dim`).
pub(crate) fn refuse_column(
    engine: &str,
    v: &dbine_driver::VectorIndex,
    column: Option<&L>,
    needs_dim: bool,
) -> Result<(), String> {
    match column {
        Some(L::Vector { sparse: true, .. }) => {
            Err(format!("{engine} no indexa vectores dispersos"))
        }
        Some(L::Vector { .. }) if needs_dim && searched_dims(v, column).is_none() => Err(format!(
            "{engine} necesita la dimensión de los vectores y no se conoce"
        )),
        _ => Ok(()),
    }
}

/// Another family's index parameters this family takes (`keep`), plus the
/// dimension of the vectors it searches ([`searched_dims`]), under `dim`.
pub(crate) fn index_params(
    v: &dbine_driver::VectorIndex,
    keep: &[&str],
    column: Option<&L>,
) -> std::collections::BTreeMap<String, String> {
    let mut out: std::collections::BTreeMap<String, String> = v
        .params
        .iter()
        .filter(|(k, _)| keep.contains(&k.as_str()))
        .map(|(k, val)| (k.clone(), val.clone()))
        .collect();
    if let Some(n) = searched_dims(v, column) {
        out.insert("dim".into(), n.to_string());
    }
    out
}

/// How a target engine writes its vectors, for [`settle_vectors`].
pub(crate) struct VectorTarget<'a> {
    pub engine: &'a str,
    /// The engine's vector type of `n` dimensions (or without one, where
    /// the dimension goes in the index).
    pub ty: &'a dyn Fn(Option<u32>) -> String,
    /// Whether a column type is the engine's vector type, with its
    /// dimension when the type says it.
    pub is_vector: &'a dyn Fn(&str) -> Option<Option<u32>>,
    /// The index parameter that holds the dimension (Doris / StarRocks
    /// `dim`, Manticore `knn_dims`); `None` where the type holds it.
    pub dim_param: Option<&'a str>,
    /// A vector index can't be made without the dimension.
    pub needs_dim: bool,
    /// What a vector without a dimension becomes when nothing says it
    /// (`None`: the engine's vector type takes none).
    pub fallback: Option<Rendered>,
}

/// The source column a target column came from (convert marks them while
/// `finalize` runs), for the issues already said about it.
fn origin(c: &dbine_driver::ColumnDef) -> &str {
    c.options
        .get("__dbine_source_column")
        .map_or(c.name.as_str(), String::as_str)
}

/// The type notes said about a column, now that its type changed again.
fn forget_type_notes(report: &mut Report, column: &str) {
    report.issues.retain(|i| {
        !(matches!(i.code, IssueCode::TypeApproximated | IssueCode::TypeChanged)
            && i.object
                .as_deref()
                .is_some_and(|o| o.eq_ignore_ascii_case(column)))
    });
}

/// Vector columns and indexes on a target engine, in `finalize`:
/// - a vector index's column becomes the engine's vector type: with the
///   dimension of its type, else the one the index holds (Doris' `dim`,
///   Manticore's `knn_dims`, or the column's, put there by
///   [`Dialect::vector_index`]). `convert` already turned a list of numbers
///   the source searched with a stated dimension (Doris' `ARRAY<FLOAT>`)
///   into a vector;
/// - where the engine keeps the dimension in the index, it goes there;
/// - an index whose column can't be a vector (or lacks the dimension the
///   engine needs) is dropped and said, never written on another type;
/// - a vector left without a dimension takes the fallback.
pub(crate) fn settle_vectors(t: &mut TableSchema, report: &mut Report, target: &VectorTarget) {
    use IssueCode::*;
    use Severity::*;
    let engine = target.engine;
    let table = t.name.clone();
    // The marks the rendered types carry.
    let mut marked: std::collections::HashMap<String, Option<u32>> = Default::default();
    for c in &mut t.columns {
        if let (ty, Some(n)) = take_dim(&c.data_type) {
            c.data_type = ty;
            marked.insert(c.name.clone(), n);
        }
    }
    let mut settled: std::collections::HashSet<String> = Default::default();
    let mut kept = Vec::new();
    for mut ix in std::mem::take(&mut t.indexes) {
        let Some(vi) = ix.vector.as_mut() else {
            kept.push(ix);
            continue;
        };
        let hint = index_dim(vi);
        for k in DIM_PARAMS {
            vi.params.remove(*k);
        }
        let name = ix.name.clone();
        let drop = |report: &mut Report, why: String| {
            report.push(
                Dropped,
                IndexDropped,
                &table,
                Some(&name),
                format!("El índice vectorial «{name}» no se crea: {why}."),
            );
        };
        let [col] = ix.columns.as_slice() else {
            drop(
                report,
                format!("en {engine} un índice vectorial lleva una sola columna, la del vector"),
            );
            continue;
        };
        let Some(c) = t.columns.iter_mut().find(|c| c.name == *col) else {
            drop(report, format!("no está su columna «{col}»"));
            continue;
        };
        let typed = marked
            .get(&c.name)
            .copied()
            .or_else(|| (target.is_vector)(&c.data_type));
        let n = typed.flatten().or(hint);
        match (typed, n) {
            (Some(own), Some(n)) => {
                if own.is_none() {
                    forget_type_notes(report, origin(c));
                    report.push(
                        Info,
                        TypeChanged,
                        &table,
                        Some(origin(c)),
                        format!(
                            "La dimensión del vector ({n}) sale de su índice vectorial «{name}»."
                        ),
                    );
                }
                c.data_type = (target.ty)(Some(n));
            }
            (Some(_), None) if !target.needs_dim => {}
            (Some(_), None) => {
                drop(
                    report,
                    format!("{engine} necesita la dimensión de los vectores y no se conoce"),
                );
                continue;
            }
            // A list of numbers the source searched with a stated dimension
            // already came as a vector (`convert`): anything else isn't one.
            (None, _) => {
                drop(
                    report,
                    format!("su columna «{col}» no es un vector en {engine}"),
                );
                continue;
            }
        }
        settled.insert(c.name.clone());
        if let (Some(k), Some(n), Some(vi)) = (target.dim_param, n, ix.vector.as_mut()) {
            vi.params.insert(k.to_string(), n.to_string());
        }
        kept.push(ix);
    }
    t.indexes = kept;
    // Vectors no index gave a dimension to.
    for c in &mut t.columns {
        match marked.get(&c.name) {
            Some(Some(n)) if !settled.contains(&c.name) => c.data_type = (target.ty)(Some(*n)),
            Some(None) if !settled.contains(&c.name) => {
                if let Some(fb) = &target.fallback {
                    c.data_type = fb.native.clone();
                    for note in &fb.notes {
                        report.push(
                            note.severity,
                            note.code,
                            &table,
                            Some(origin(c)),
                            note.message.clone(),
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

/// On an engine whose secondary indexes are its vector ones and its `own`
/// kinds: the rest are dropped and said.
pub(crate) fn drop_plain_indexes(t: &mut TableSchema, report: &mut Report, own: &[&str]) {
    let table = t.name.clone();
    t.indexes.retain(|ix| {
        let keep = ix.vector.is_some()
            || ix
                .kind
                .as_deref()
                .is_some_and(|k| own.iter().any(|o| o.eq_ignore_ascii_case(k)));
        if !keep {
            report.push(
                Severity::Dropped,
                IssueCode::IndexDropped,
                &table,
                Some(&ix.name),
                "El destino no tiene índices secundarios.",
            );
        }
        keep
    });
}

/// A nested type spelling (an array element, a map value) in dialect `d`.
pub(crate) fn nested(d: &dyn Dialect, native: &str) -> L {
    crate::convert::logical_of(d, &parse::parse(native))
}

/// The common angle-bracket containers: `ARRAY<T>` / `LIST<T>`, `MAP<K, V>`,
/// and `STRUCT<…>` (no logical struct: JSON).
pub(crate) fn parse_container(d: &dyn Dialect, raw: &str) -> Option<L> {
    let (name, args) = angle(raw)?;
    Some(match (name.as_str(), args.as_slice()) {
        ("array" | "list", [of]) => L::Array {
            of: Box::new(nested(d, of)),
        },
        ("map", [k, v]) => L::Map {
            key: Box::new(nested(d, k)),
            value: Box::new(nested(d, v)),
        },
        ("struct" | "row" | "tuple", _) => L::Json { binary: false },
        _ => L::Other {
            native: raw.to_string(),
        },
    })
}

/// Rendered element type inside a container, notes carried over.
pub(crate) fn wrap(inner: Rendered, f: impl FnOnce(&str) -> String) -> Rendered {
    Rendered {
        native: f(&inner.native),
        notes: inner.notes,
    }
}

pub(crate) fn merge(a: Rendered, b: Rendered, f: impl FnOnce(&str, &str) -> String) -> Rendered {
    let mut notes = a.notes;
    notes.extend(b.notes);
    Rendered {
        native: f(&a.native, &b.native),
        notes,
    }
}

/// The source says nothing about the precision of a numeric: the widest
/// sensible decimal of a 38-digit engine.
pub(crate) fn unbounded_decimal(engine: &str) -> Rendered {
    Rendered::exact("DECIMAL(38, 10)").with(
        Severity::Loss,
        IssueCode::PrecisionLoss,
        format!("El origen no fija la precisión: se usa DECIMAL(38, 10), dentro del máximo de {engine}."),
    )
}

/// DECIMAL(p, s) capped at `max` digits.
pub(crate) fn capped_decimal(p: u32, s: Option<u32>, max: u32, engine: &str) -> Rendered {
    let s = s.unwrap_or(0);
    if p <= max && s <= p {
        Rendered::exact(format!("DECIMAL({p}, {s})"))
    } else {
        Rendered::exact(format!("DECIMAL({max}, {})", s.min(max))).with(
            Severity::Loss,
            IssueCode::PrecisionLoss,
            format!("{engine} admite hasta {max} dígitos; el origen es ({p}, {s})."),
        )
    }
}

impl Olap {
    fn name(&self) -> &'static str {
        if self.doris {
            "Doris"
        } else {
            "StarRocks"
        }
    }

    fn max_varchar(&self) -> u32 {
        if self.doris {
            DORIS_MAX_VARCHAR
        } else {
            SR_MAX_VARCHAR
        }
    }

    /// Unbounded text: Doris STRING (up to 2 GB), StarRocks' widest VARCHAR.
    fn text(&self) -> Rendered {
        if self.doris {
            Rendered::exact("STRING")
        } else {
            Rendered::exact(format!("VARCHAR({SR_MAX_VARCHAR})")).with(
                Severity::Warning,
                IssueCode::LengthLoss,
                "StarRocks guarda hasta 1 MB por valor de texto.",
            )
        }
    }

    /// A text of `len` characters as a byte-length VARCHAR.
    fn varchar(&self, len: u32, unicode: bool) -> Rendered {
        let bytes = if unicode {
            len.saturating_mul(UTF8_BYTES)
        } else {
            len
        };
        if bytes <= self.max_varchar() {
            let r = Rendered::exact(format!("VARCHAR({bytes})"));
            if unicode {
                r.with(
                    Severity::Info,
                    IssueCode::TypeChanged,
                    format!(
                        "{} mide VARCHAR en bytes: {len} caracteres son hasta {bytes} bytes.",
                        self.name()
                    ),
                )
            } else {
                r
            }
        } else if self.doris {
            Rendered::exact("STRING")
        } else if len <= SR_MAX_VARCHAR {
            Rendered::exact(format!("VARCHAR({SR_MAX_VARCHAR})")).with(
                Severity::Info,
                IssueCode::TypeChanged,
                "StarRocks mide VARCHAR en bytes: se usa el máximo (1 MB).",
            )
        } else {
            self.text()
        }
    }

    fn binary(&self) -> Rendered {
        if self.doris {
            Rendered::exact("STRING").with(
                Severity::Warning,
                IssueCode::TypeApproximated,
                "Doris no tiene un tipo binario para tablas: los bytes se guardan como STRING.",
            )
        } else {
            Rendered::exact("VARBINARY")
        }
    }

    fn as_text(&self, why: &str) -> Rendered {
        self.text().with(
            Severity::Warning,
            IssueCode::TypeApproximated,
            why.to_string(),
        )
    }
}

impl Dialect for Olap {
    fn id(&self) -> &'static str {
        if self.doris {
            "doris"
        } else {
            "starrocks"
        }
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        if let Some(c) = parse_container(self, &t.raw) {
            return c;
        }
        let p = |i| t.arg_u32(i);
        let prec = || p(0).map(|x| x.min(9) as u8);
        match t.name.as_str() {
            "boolean" | "bool" => L::Bool,
            "tinyint" if p(0) == Some(1) => L::Bool,
            "tinyint" => L::int(1),
            "smallint" => L::int(2),
            "int" | "integer" => L::int(4),
            // information_schema spells LARGEINT `bigint(20) unsigned`: no
            // unsigned types here, so that is the 128-bit integer.
            "bigint" if t.unsigned => L::int(16),
            "bigint" => L::int(8),
            "largeint" => L::int(16),
            "decimal" | "decimalv2" | "decimalv3" | "decimal32" | "decimal64" | "decimal128"
            | "decimal256" | "numeric" => L::Decimal {
                precision: p(0).or(Some(10)),
                scale: p(1).or(Some(0)),
            },
            "float" => L::Float { bytes: 4 },
            "double" => L::Float { bytes: 8 },
            "char" => L::Char {
                len: p(0).or(Some(1)),
                unicode: true,
            },
            // STRING is VARCHAR(65533) in the catalog.
            "varchar" if p(0).is_none_or(|n| n >= DORIS_MAX_VARCHAR) => L::Text { unicode: true },
            "varchar" => L::Varchar {
                len: p(0),
                unicode: true,
            },
            "string" | "text" => L::Text { unicode: true },
            "binary" | "varbinary" => match p(0) {
                Some(n) if n < SR_MAX_VARCHAR => L::Varbinary { len: Some(n) },
                _ => L::Blob,
            },
            "date" | "datev2" => L::Date,
            "datetime" | "datetimev2" => L::Timestamp {
                precision: prec().or(if self.doris { Some(0) } else { Some(6) }),
                tz: false,
            },
            "time" | "timev2" => L::Time {
                precision: prec(),
                tz: false,
            },
            "json" | "jsonb" | "variant" => L::Json { binary: true },
            "ipv4" | "ipv6" => L::Inet,
            _ => L::Other {
                native: t.raw.clone(),
            },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let engine = self.name();
        match t {
            L::Vector { sparse: true, .. } => super::vector_fallback(self, t),
            // The vector index's column: ARRAY<FLOAT>, its dimension in
            // the index (`dim`, from `vector_index`).
            L::Vector { element, .. } => {
                let r = Rendered::exact("ARRAY<FLOAT>");
                if *element == dbine_driver::VectorElement::F32 {
                    r
                } else {
                    r.with(
                        Warning,
                        TypeApproximated,
                        format!("{}: se guarda como float32.", t.describe()),
                    )
                }
            }
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => {
                let b = L::signed_bytes_for(*bytes, *unsigned);
                let r = match b {
                    1 => Rendered::exact("TINYINT"),
                    2 => Rendered::exact("SMALLINT"),
                    3 | 4 => Rendered::exact("INT"),
                    8 => Rendered::exact("BIGINT"),
                    _ => Rendered::exact("LARGEINT"),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, format!("{engine} no tiene enteros sin signo: se usa un tipo más grande con signo."))
                } else {
                    r
                }
            }
            L::Decimal {
                precision: Some(p),
                scale,
            } => capped_decimal(*p, *scale, 38, engine),
            L::Decimal {
                precision: None, ..
            } => unbounded_decimal(engine),
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(
                Info,
                TypeChanged,
                "Moneda como DECIMAL(19, 4).",
            ),
            L::Char { len, unicode } => {
                let n = len.unwrap_or(1);
                if !unicode && n <= 255 {
                    Rendered::exact(format!("CHAR({n})"))
                } else {
                    // CHAR is in bytes too (up to 255): a variable byte length is safer.
                    self.varchar(n, *unicode).with(
                        Info,
                        TypeChanged,
                        "CHAR pasa a VARCHAR (sin relleno de espacios).",
                    )
                }
            }
            L::Varchar {
                len: Some(n),
                unicode,
            } => self.varchar(*n, *unicode),
            L::Varchar { len: None, .. } | L::Text { .. } => self.text(),
            L::Binary { len } | L::Varbinary { len } if !self.doris => match len {
                Some(n) if *n <= SR_MAX_VARCHAR => Rendered::exact(format!("VARBINARY({n})")),
                _ => Rendered::exact("VARBINARY"),
            },
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => self.binary(),
            L::Bit { len } => match len {
                Some(n) if *n <= 63 => {
                    Rendered::exact("BIGINT").with(Info, TypeChanged, "Cadena de bits como entero.")
                }
                Some(n) if *n <= 127 => Rendered::exact("LARGEINT").with(
                    Info,
                    TypeChanged,
                    "Cadena de bits como entero.",
                ),
                _ => self.binary().with(
                    Warning,
                    TypeApproximated,
                    "Cadena de bits larga: se guarda como binario.",
                ),
            },
            L::Date => Rendered::exact("DATE"),
            L::Time { tz, .. } => {
                let r = Rendered::exact("VARCHAR(32)").with(
                    Warning,
                    TypeApproximated,
                    format!("{engine} no tiene columnas de hora: queda como texto HH:MM:SS."),
                );
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Se pierde la zona horaria de la hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let r = if self.doris {
                    match precision {
                        Some(p) if *p > 0 => Rendered::exact(format!("DATETIME({})", (*p).min(6)))
                            .with_loss(precision_loss(Some(*p), 6)),
                        Some(_) => Rendered::exact("DATETIME"),
                        None => Rendered::exact("DATETIME(6)"),
                    }
                } else {
                    Rendered::exact("DATETIME").with_loss(precision_loss(*precision, 6))
                };
                if *tz {
                    r.with(Loss, TimeZoneLoss, format!("{engine} no guarda la zona horaria: queda como DATETIME en la zona de la sesión."))
                } else {
                    r
                }
            }
            L::Interval => Rendered::exact("VARCHAR(64)").with(
                Warning,
                TypeApproximated,
                format!("{engine} no tiene intervalos: queda como texto."),
            ),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            L::Uuid => {
                Rendered::exact("VARCHAR(36)").with(Info, TypeChanged, "UUID como VARCHAR(36).")
            }
            L::Json { .. } => Rendered::exact("JSON"),
            L::Xml => self.as_text("XML como texto."),
            L::Enum { values } => self.varchar(longest(values) as u32, true).with(
                Warning,
                TypeApproximated,
                format!(
                    "{engine} no tiene enumerados: queda como texto. Valores: {}.",
                    values.join(", ")
                ),
            ),
            L::Set { values } => Rendered::exact("ARRAY<VARCHAR(255)>").with(
                Warning,
                TypeApproximated,
                format!(
                    "Conjunto como arreglo de texto. Valores: {}.",
                    values.join(", ")
                ),
            ),
            L::Array { of } => wrap(self.render_type(of), |i| format!("ARRAY<{i}>")),
            L::Map { key, value } => {
                merge(self.render_type(key), self.render_type(value), |k, v| {
                    format!("MAP<{k}, {v}>")
                })
            }
            L::Geometry { .. } => self.as_text("Dato espacial como texto (WKT)."),
            L::Inet => Rendered::exact("VARCHAR(45)").with(
                Info,
                TypeApproximated,
                "Dirección IP como texto.",
            ),
            L::MacAddr => Rendered::exact("VARCHAR(17)").with(
                Info,
                TypeApproximated,
                "Dirección MAC como texto.",
            ),
            L::RowVersion => Rendered::exact("BIGINT").with(
                Warning,
                TypeApproximated,
                format!("{engine} no tiene versión de fila automática: no se actualiza sola."),
            ),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    /// Defaults are constants (the driver quotes them) or CURRENT_TIMESTAMP
    /// on DATETIME; StarRocks also takes `uuid()` on VARCHAR.
    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            // No explicit NULL default: the driver would quote it as text.
            DefaultValue::Null | DefaultValue::NextVal(_) | DefaultValue::Expr(_) => return None,
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => quote(s),
            DefaultValue::Bool(b) => if *b { "1" } else { "0" }.into(),
            DefaultValue::CurrentTimestamp if matches!(ty, L::Timestamp { .. }) => {
                "CURRENT_TIMESTAMP".into()
            }
            DefaultValue::NewUuid
                if !self.doris
                    && matches!(
                        ty,
                        L::Uuid | L::Varchar { .. } | L::Char { .. } | L::Text { .. }
                    ) =>
            {
                "uuid()".into()
            }
            _ => return None,
        })
    }

    /// Doris' ANN index (hnsw / ivf; L2 and inner product) and StarRocks'
    /// vector index (hnsw / ivfpq; L2 and cosine), with the dimension
    /// (`dim`) of the column it searches.
    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
        column: Option<&crate::logical::LogicalType>,
    ) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
        use dbine_driver::vector::{metric, param};
        let engine = self.name();
        refuse_column(engine, v, column, true)?;
        let metrics: &[&str] = if self.doris {
            &[metric::L2, metric::IP]
        } else {
            &[metric::L2, metric::COSINE]
        };
        let m = v.metric.as_deref().unwrap_or(metric::L2);
        if !metrics.contains(&m) {
            return Err(format!(
                "{engine} no tiene la distancia «{m}» en sus índices vectoriales (tiene {})",
                metrics.join(", ")
            ));
        }
        let ivf = v
            .algorithm
            .as_deref()
            .is_some_and(|a| a.to_ascii_lowercase().starts_with("ivf"));
        let algorithm = match (ivf, self.doris) {
            (false, _) => "hnsw",
            (true, true) => "ivf",
            (true, false) => "ivfpq",
        };
        let params = index_params(v, &[param::M, param::EF_CONSTRUCTION, param::LISTS], column);
        Ok((
            Some(if self.doris { "ANN" } else { "VECTOR" }.into()),
            dbine_driver::VectorIndex {
                algorithm: Some(algorithm.into()),
                metric: Some(m.into()),
                params,
            },
        ))
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: false,
            on_delete: &[],
            on_update: &[],
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            // AUTO_INCREMENT exists in StarRocks 3.0+, but the designer leaves it out.
            auto_increment: false,
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 64,
            case: IdentCase::Preserve,
            vector_indexes: true,
            vectors: true,
        }
    }

    /// Key model, key columns and distribution from the primary key, in the
    /// designer's option keys (`key_model`, `key_columns`, `distributed_by`,
    /// `replication_num`); indexes the engine can't build are left out.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        use IssueCode::*;
        use Severity::*;
        let engine = self.name();
        let table = t.name.clone();
        let native_of = |t: &TableSchema, c: &str| {
            t.columns
                .iter()
                .find(|x| x.name == c)
                .map(|x| parse::parse(&x.data_type))
        };
        let pk: Vec<String> = t
            .primary_key
            .as_ref()
            .map(|k| k.columns.clone())
            .unwrap_or_default();

        // Vector indexes: on an ARRAY<FLOAT> column, with its dimension
        // (`dim`) in the index; the column can't be NULL. Settled before the
        // key model, which a vector index makes DUPLICATE KEY: only the ones
        // that are created count.
        settle_vectors(
            t,
            report,
            &VectorTarget {
                engine,
                ty: &|_| "ARRAY<FLOAT>".to_string(),
                is_vector: &|ty| {
                    matches!(
                        self.parse_type(&parse::parse(ty)),
                        L::Array { of } if *of == L::Float { bytes: 4 }
                    )
                    .then_some(None)
                },
                dim_param: Some("dim"),
                needs_dim: true,
                fallback: None,
            },
        );
        for ix in &t.indexes {
            let Some(col) = ix.columns.first().filter(|_| ix.vector.is_some()) else {
                continue;
            };
            if let Some(c) = t.columns.iter_mut().find(|c| c.name == *col && c.nullable) {
                c.nullable = false;
                report.push(
                    Info,
                    NullabilityChanged,
                    &table,
                    Some(col),
                    format!("{engine} indexa vectores de una columna NOT NULL: la columna queda NOT NULL."),
                );
            }
        }

        if !t.options.contains_key("key_model") {
            // StarRocks' PRIMARY KEY takes integers, strings, dates and booleans;
            // UNIQUE / DUPLICATE sort keys also take decimals.
            let bad_for = |primary: bool| {
                pk.iter()
                    .find(|c| native_of(t, c).is_none_or(|s| !key_type(&s, primary, self.doris)))
                    .cloned()
            };
            let (model, keys) = if pk.is_empty() {
                let first = t
                    .columns
                    .iter()
                    .find(|c| key_type(&parse::parse(&c.data_type), false, self.doris))
                    .map(|c| c.name.clone());
                report.push(
                    Info,
                    OptionAdded,
                    &table,
                    Some("key_model"),
                    format!(
                        "Sin clave primaria: tabla DUPLICATE KEY{}, que admite filas repetidas.",
                        first
                            .as_deref()
                            .map(|f| format!(" ordenada por «{f}»"))
                            .unwrap_or_default()
                    ),
                );
                ("duplicate", first.into_iter().collect::<Vec<_>>())
            } else if t.indexes.iter().any(|i| i.vector.is_some())
                && bad_for(false).is_none()
                && (self.doris || bad_for(true).is_some())
            {
                // Doris builds ANN indexes on DUPLICATE KEY tables only,
                // StarRocks its vector indexes on DUPLICATE or PRIMARY KEY
                // (not UNIQUE, where a key PRIMARY KEY can't hold ends up).
                let list = pk.join(", ");
                let why = if self.doris {
                    "Doris crea índices ANN solo en tablas DUPLICATE KEY"
                } else {
                    "StarRocks crea índices vectoriales solo en tablas DUPLICATE KEY o PRIMARY KEY, y esta clave no entra en PRIMARY KEY"
                };
                report.push(Warning, PrimaryKeyDropped, &table, Some(&list), format!(
                    "{why}: la tabla queda DUPLICATE KEY({list}) y no garantiza la unicidad de su clave primaria."
                ));
                t.primary_key = None;
                ("duplicate", pk.clone())
            } else if !self.doris && bad_for(true).is_none() {
                ("primary", pk.clone())
            } else if bad_for(false).is_none() {
                if !self.doris {
                    report.push(Info, OptionAdded, &table, Some("key_model"), "La clave primaria tiene columnas que PRIMARY KEY no admite (decimales): se usa UNIQUE KEY.");
                }
                ("unique", pk.clone())
            } else {
                let bad = bad_for(false).unwrap_or_default();
                let first = t
                    .columns
                    .iter()
                    .find(|c| key_type(&parse::parse(&c.data_type), false, self.doris))
                    .map(|c| c.name.clone());
                report.push(Loss, PrimaryKeyDropped, &table, Some(&bad), format!(
                    "{engine} no admite la columna «{bad}» en una clave: la tabla queda DUPLICATE KEY y no garantiza unicidad."
                ));
                t.primary_key = None;
                ("duplicate", first.into_iter().collect())
            };
            t.options.insert("key_model".into(), model.into());
            if !keys.is_empty() {
                let list = keys.join(", ");
                t.options.insert("key_columns".into(), list.clone());
                if !t.options.contains_key("distributed_by") {
                    t.options.insert("distributed_by".into(), list.clone());
                    report.push(Info, OptionAdded, &table, Some("distributed_by"), format!("Distribución DISTRIBUTED BY HASH({list}), por las columnas de la clave."));
                }
                if model != "duplicate" {
                    report.push(Info, OptionAdded, &table, Some("key_model"), format!("Modelo {} KEY({list}): las columnas de la clave van primero en la tabla.", model.to_ascii_uppercase()));
                }
            }
            // Doris keys can't be STRING: its VARCHAR maximum instead.
            if self.doris {
                for c in t.columns.iter_mut().filter(|c| {
                    keys.contains(&c.name) && c.data_type.eq_ignore_ascii_case("STRING")
                }) {
                    c.data_type = format!("VARCHAR({DORIS_MAX_VARCHAR})");
                    report.push(
                        Info,
                        TypeChanged,
                        &table,
                        Some(&c.name),
                        "Doris no admite STRING en la clave: se usa VARCHAR(65533).",
                    );
                }
            }
        }
        if !t.options.contains_key("replication_num") {
            t.options.insert("replication_num".into(), "1".into());
            report.push(
                Warning,
                OptionAdded,
                &table,
                Some("replication_num"),
                "Una réplica (lo que propone el diseñador): subila en un clúster de varios nodos.",
            );
        }

        // One-column, non-unique indexes on scalar columns only (and the
        // vector indexes, on their ARRAY<FLOAT> column, with their dim).
        let mut kept = Vec::new();
        for ix in std::mem::take(&mut t.indexes) {
            // Vector indexes come settled (their dim in them).
            let why = if ix.vector.is_some() {
                None
            } else if ix.unique {
                Some(format!("{engine} no tiene índices únicos: la unicidad solo la da la clave de la tabla."))
            } else if ix.columns.len() != 1 {
                Some(format!("Los índices de {engine} son de una sola columna."))
            } else if native_of(t, &ix.columns[0]).is_none_or(|s| !indexable(&s)) {
                Some(format!("{engine} no indexa columnas de ese tipo."))
            } else {
                None
            };
            match why {
                Some(w) => report.push(Dropped, IndexDropped, &table, Some(&ix.name), w),
                None => kept.push(ix),
            }
        }
        t.indexes = kept;
    }
}

/// Whether a column typed `s` can be a key column (`primary`: StarRocks'
/// PRIMARY KEY model, stricter).
fn key_type(s: &TypeSpec, primary: bool, doris: bool) -> bool {
    match s.name.as_str() {
        "boolean" | "tinyint" | "smallint" | "int" | "bigint" | "largeint" | "date"
        | "datetime" | "varchar" => true,
        "char" => !primary,
        "decimal" => !primary,
        "string" => !doris,
        _ => false,
    }
}

fn indexable(s: &TypeSpec) -> bool {
    key_type(s, false, false) || s.name == "string"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn vectors_take_an_ann_or_vector_index_with_their_dimension() {
        use dbine_driver::{ColumnDef, IndexDef, VectorElement, VectorIndex};
        let v3 = L::Vector {
            dims: Some(3),
            element: VectorElement::F32,
            sparse: false,
        };
        assert_eq!(doris().render_type(&v3).native, "ARRAY<FLOAT>");
        // A pgvector HNSW cosine index: StarRocks has cosine, Doris doesn't.
        let pg = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("cosine".into()),
            params: [("m".to_string(), "16".to_string())].into(),
        };
        let (k, v) = sr().vector_index(&pg, None).unwrap();
        assert_eq!(k.as_deref(), Some("VECTOR"));
        assert_eq!(v.algorithm.as_deref(), Some("hnsw"));
        assert!(doris().vector_index(&pg, None).is_err());
        let l2 = VectorIndex {
            metric: Some("l2".into()),
            algorithm: Some("ivfflat".into()),
            ..pg.clone()
        };
        let (k, v) = doris().vector_index(&l2, Some(&v3)).unwrap();
        assert_eq!(
            (k.as_deref(), v.algorithm.as_deref()),
            (Some("ANN"), Some("ivf"))
        );
        // The dimension is the searched column's.
        assert_eq!(v.params.get("dim").map(String::as_str), Some("3"));
        // A vector of unknown dimension, or a sparse one: no index.
        let any = L::Vector {
            dims: None,
            element: VectorElement::F32,
            sparse: false,
        };
        let e = sr().vector_index(&l2, Some(&any)).unwrap_err();
        assert!(e.contains("necesita la dimensión"), "{e}");
        let sparse = L::Vector {
            dims: Some(30_000),
            element: VectorElement::F32,
            sparse: true,
        };
        let e = doris().vector_index(&l2, Some(&sparse)).unwrap_err();
        assert!(e.contains("dispersos"), "{e}");
        // finalize: the dim goes into the index, the column NOT NULL.
        let mut t = TableSchema {
            name: "items".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: "INT".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnDef {
                    name: "v".into(),
                    data_type: "ARRAY<FLOAT>".into(),
                    nullable: true,
                    ..Default::default()
                },
            ],
            indexes: vec![IndexDef {
                name: "iv".into(),
                columns: vec!["v".into()],
                kind: k,
                vector: Some(v),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut report = Report::default();
        doris().finalize(&mut t, &mut report);
        assert_eq!(t.columns[1].data_type, "ARRAY<FLOAT>");
        assert!(!t.columns[1].nullable);
        assert_eq!(t.indexes.len(), 1);
        assert_eq!(
            t.indexes[0]
                .vector
                .as_ref()
                .unwrap()
                .params
                .get("dim")
                .map(String::as_str),
            Some("3")
        );
    }

    fn sr() -> &'static dyn Dialect {
        lookup("starrocks").unwrap()
    }
    fn doris() -> &'static dyn Dialect {
        lookup("velodb").unwrap()
    }
    fn p(d: &dyn Dialect, s: &str) -> L {
        crate::convert::logical_of(d, &parse(s))
    }

    #[test]
    fn parses_catalog_spellings() {
        let d = sr();
        assert_eq!(p(d, "tinyint(1)"), L::Bool);
        assert_eq!(p(d, "tinyint(4)"), L::int(1));
        assert_eq!(p(d, "smallint(6)"), L::int(2));
        assert_eq!(p(d, "int(11)"), L::int(4));
        assert_eq!(p(d, "bigint(20)"), L::int(8));
        assert_eq!(p(d, "bigint(20) unsigned"), L::int(16));
        assert_eq!(p(d, "largeint"), L::int(16));
        assert_eq!(
            p(d, "decimal(10, 2)"),
            L::Decimal {
                precision: Some(10),
                scale: Some(2)
            }
        );
        assert_eq!(
            p(d, "decimal64(18,4)"),
            L::Decimal {
                precision: Some(18),
                scale: Some(4)
            }
        );
        assert_eq!(p(d, "float"), L::Float { bytes: 4 });
        assert_eq!(p(d, "double"), L::Float { bytes: 8 });
        assert_eq!(
            p(d, "char(10)"),
            L::Char {
                len: Some(10),
                unicode: true
            }
        );
        assert_eq!(
            p(d, "varchar(20)"),
            L::Varchar {
                len: Some(20),
                unicode: true
            }
        );
        assert_eq!(p(d, "varchar(65533)"), L::Text { unicode: true });
        assert_eq!(p(d, "varchar(1048576)"), L::Text { unicode: true });
        assert_eq!(p(d, "string"), L::Text { unicode: true });
        assert_eq!(p(d, "varbinary(16)"), L::Varbinary { len: Some(16) });
        assert_eq!(p(d, "date"), L::Date);
        assert_eq!(
            p(d, "datetime"),
            L::Timestamp {
                precision: Some(6),
                tz: false
            }
        );
        assert_eq!(
            p(doris(), "datetime(3)"),
            L::Timestamp {
                precision: Some(3),
                tz: false
            }
        );
        assert_eq!(
            p(doris(), "datetimev2(0)"),
            L::Timestamp {
                precision: Some(0),
                tz: false
            }
        );
        assert_eq!(p(d, "json"), L::Json { binary: true });
        assert_eq!(
            p(d, "array<int(11)>"),
            L::Array {
                of: Box::new(L::int(4))
            }
        );
        assert_eq!(
            p(d, "map<varchar(10),int(11)>"),
            L::Map {
                key: Box::new(L::Varchar {
                    len: Some(10),
                    unicode: true
                }),
                value: Box::new(L::int(4))
            }
        );
        assert_eq!(
            p(d, "struct<`x` int(11), `y` varchar(5)>"),
            L::Json { binary: false }
        );
        assert_eq!(p(doris(), "ipv6"), L::Inet);
        assert!(matches!(p(d, "bitmap"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let d = sr();
        let r = |t: L| d.render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(
            r(L::Int {
                bytes: 4,
                unsigned: true
            }),
            "BIGINT"
        );
        assert_eq!(
            r(L::Int {
                bytes: 8,
                unsigned: true
            }),
            "LARGEINT"
        );
        assert_eq!(
            r(L::Decimal {
                precision: Some(12),
                scale: Some(2)
            }),
            "DECIMAL(12, 2)"
        );
        assert_eq!(
            r(L::Decimal {
                precision: Some(50),
                scale: Some(2)
            }),
            "DECIMAL(38, 2)"
        );
        assert_eq!(
            r(L::Decimal {
                precision: None,
                scale: None
            }),
            "DECIMAL(38, 10)"
        );
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Money), "DECIMAL(19, 4)");
        assert_eq!(
            r(L::Char {
                len: Some(3),
                unicode: false
            }),
            "CHAR(3)"
        );
        assert_eq!(
            r(L::Char {
                len: Some(3),
                unicode: true
            }),
            "VARCHAR(12)"
        );
        assert_eq!(
            r(L::Varchar {
                len: Some(20),
                unicode: true
            }),
            "VARCHAR(80)"
        );
        assert_eq!(
            r(L::Varchar {
                len: Some(20),
                unicode: false
            }),
            "VARCHAR(20)"
        );
        assert_eq!(r(L::Text { unicode: true }), "VARCHAR(1048576)");
        assert_eq!(
            doris().render_type(&L::Text { unicode: true }).native,
            "STRING"
        );
        assert_eq!(
            doris()
                .render_type(&L::Varchar {
                    len: Some(20000),
                    unicode: true
                })
                .native,
            "STRING"
        );
        assert_eq!(r(L::Binary { len: Some(16) }), "VARBINARY(16)");
        assert_eq!(r(L::Blob), "VARBINARY");
        assert_eq!(doris().render_type(&L::Blob).native, "STRING");
        assert_eq!(r(L::Bit { len: Some(8) }), "BIGINT");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(
            r(L::Time {
                precision: None,
                tz: false
            }),
            "VARCHAR(32)"
        );
        assert_eq!(
            r(L::Timestamp {
                precision: Some(3),
                tz: false
            }),
            "DATETIME"
        );
        assert_eq!(
            doris()
                .render_type(&L::Timestamp {
                    precision: Some(3),
                    tz: false
                })
                .native,
            "DATETIME(3)"
        );
        assert_eq!(
            doris()
                .render_type(&L::Timestamp {
                    precision: None,
                    tz: false
                })
                .native,
            "DATETIME(6)"
        );
        let tz = d.render_type(&L::Timestamp {
            precision: Some(9),
            tz: true,
        });
        assert!(tz.notes.iter().any(|n| n.code == IssueCode::TimeZoneLoss));
        assert!(tz.notes.iter().any(|n| n.code == IssueCode::PrecisionLoss));
        assert_eq!(r(L::Interval), "VARCHAR(64)");
        assert_eq!(r(L::Year), "SMALLINT");
        assert_eq!(r(L::Uuid), "VARCHAR(36)");
        assert_eq!(r(L::Json { binary: false }), "JSON");
        assert_eq!(r(L::Xml), "VARCHAR(1048576)");
        assert_eq!(
            r(L::Enum {
                values: vec!["a".into(), "bb".into()]
            }),
            "VARCHAR(8)"
        );
        assert_eq!(
            r(L::Set {
                values: vec!["a".into()]
            }),
            "ARRAY<VARCHAR(255)>"
        );
        assert_eq!(
            r(L::Array {
                of: Box::new(L::int(4))
            }),
            "ARRAY<INT>"
        );
        assert_eq!(
            r(L::Map {
                key: Box::new(L::Text { unicode: true }),
                value: Box::new(L::int(8))
            }),
            "MAP<VARCHAR(1048576), BIGINT>"
        );
        assert_eq!(
            r(L::Geometry {
                kind: None,
                srid: None,
                geography: false
            }),
            "VARCHAR(1048576)"
        );
        assert_eq!(r(L::Inet), "VARCHAR(45)");
        assert_eq!(r(L::MacAddr), "VARCHAR(17)");
        assert_eq!(r(L::RowVersion), "BIGINT");
        assert_eq!(
            r(L::Other {
                native: "HLL".into()
            }),
            "HLL"
        );
    }

    #[test]
    fn defaults() {
        let d = sr();
        let ts = L::Timestamp {
            precision: None,
            tz: false,
        };
        assert_eq!(
            d.render_default(&DefaultValue::CurrentTimestamp, &ts)
                .as_deref(),
            Some("CURRENT_TIMESTAMP")
        );
        assert_eq!(
            d.render_default(&DefaultValue::CurrentTimestamp, &L::Date),
            None
        );
        assert_eq!(
            d.render_default(&DefaultValue::Bool(true), &L::Bool)
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            d.render_default(
                &DefaultValue::Text("a'b".into()),
                &L::Text { unicode: true }
            )
            .as_deref(),
            Some("'a''b'")
        );
        assert_eq!(
            d.render_default(&DefaultValue::NewUuid, &L::Uuid)
                .as_deref(),
            Some("uuid()")
        );
        assert_eq!(
            doris().render_default(&DefaultValue::NewUuid, &L::Uuid),
            None
        );
        assert_eq!(d.render_default(&DefaultValue::Null, &L::Bool), None);
    }

    #[test]
    fn angle_splits_nested() {
        assert_eq!(
            angle("MAP<STRING, ARRAY<INT>>").unwrap(),
            ("map".into(), vec!["STRING".into(), "ARRAY<INT>".into()])
        );
        assert_eq!(angle("decimal(10,2)"), None);
    }
}
