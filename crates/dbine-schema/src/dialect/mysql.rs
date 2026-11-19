//! MySQL, MariaDB and the engines that speak their type system (TiDB,
//! OceanBase, SingleStore, Aurora MySQL, Cloud SQL).
//!
//! Types arrive as `information_schema.COLUMNS.COLUMN_TYPE` spells them:
//! `int` (MySQL 8.0.19+) or `int(11)` (MariaDB, older MySQL), `tinyint(1)`,
//! `bigint(20) unsigned zerofill`, `decimal(10,2)`, `datetime(6)`,
//! `enum('a','b')`, `set(…)`; MariaDB adds `uuid`, `inet4`, `inet6` and
//! reports its JSON as `longtext`. The character set isn't part of it.

use super::postgres::{
    fit_decimal, precision_loss, set_len, single_auto_increment, uuid_slot, UuidSlot,
};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::{quote, DefaultValue};
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{parse, TypeSpec};
use dbine_driver::{IndexDef, TableSchema};

pub struct MySql;

/// A vector of `dims` (`None`: any) of the element `e` names (SingleStore's
/// `F32`, `F16`, `I8`…; float32 when not given).
fn vector_type(dims: Option<u32>, e: Option<&str>) -> L {
    use dbine_driver::VectorElement as E;
    let element = match e.map(|e| e.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("f32") | Some("float") | Some("float32") => E::F32,
        Some("f64") | Some("double") | Some("float64") => E::F64,
        Some("f16") | Some("float16") => E::F16,
        Some("i8") | Some("int8") => E::I8,
        _ => E::Other,
    };
    L::Vector {
        dims,
        element,
        sparse: false,
    }
}

/// The distances some engine of the family indexes (MariaDB: euclidean,
/// cosine; TiDB: cosine, L2; OceanBase: L2, inner product, cosine;
/// SingleStore: dot product, euclidean).
const VECTOR_METRICS: &[&str] = &["cosine", "l2", "ip"];

/// Longest `varchar(n)` in utf8mb4 (65 535 bytes per row / 4 bytes per char).
const MAX_VARCHAR: u32 = 16_383;
/// Row size limit, not counting TEXT/BLOB contents.
const MAX_ROW: u64 = 65_535;
/// InnoDB index key limit (DYNAMIC row format), in bytes.
const MAX_KEY: u64 = 3072;

impl Dialect for MySql {
    fn id(&self) -> &'static str {
        "mysql"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let frac = || p(0).map(|x| x.min(255) as u8);
        let unsigned = t.unsigned;
        // Character sets other than the unicode ones.
        let unicode = !t.rest.iter().any(|w| {
            matches!(
                w.as_str(),
                "latin1" | "ascii" | "binary" | "latin2" | "cp1252"
            )
        });
        match t.name.as_str() {
            // MySQL's BOOL is TINYINT(1), and catalogs report it that way.
            "tinyint" if p(0) == Some(1) && !unsigned => L::Bool,
            "bool" | "boolean" => L::Bool,
            "bit" if p(0).unwrap_or(1) == 1 => L::Bool,
            "bit" => L::Bit { len: p(0) },
            // Display widths (`int(11)`) don't change the size.
            "tinyint" | "int1" => L::Int { bytes: 1, unsigned },
            "smallint" | "int2" => L::Int { bytes: 2, unsigned },
            "mediumint" | "int3" | "middleint" => L::Int { bytes: 3, unsigned },
            "int" | "integer" | "int4" => L::Int { bytes: 4, unsigned },
            "bigint" | "int8" => L::Int { bytes: 8, unsigned },
            "serial" => L::Int {
                bytes: 8,
                unsigned: true,
            },
            "decimal" | "numeric" | "dec" | "fixed" => L::Decimal {
                precision: p(0).or(Some(10)),
                scale: p(1).or(Some(0)),
            },
            // FLOAT(p) chooses the size; FLOAT(M,D) is the deprecated display form.
            "float" if t.args.len() == 1 => L::Float {
                bytes: if p(0).is_some_and(|b| b > 24) { 8 } else { 4 },
            },
            "float" | "float4" => L::Float { bytes: 4 },
            "double" | "double precision" | "real" | "float8" => L::Float { bytes: 8 },
            "char" => L::Char {
                len: p(0).or(Some(1)),
                unicode,
            },
            "varchar" => L::Varchar { len: p(0), unicode },
            "tinytext" => L::Varchar {
                len: Some(255),
                unicode,
            },
            "text" | "mediumtext" | "longtext" | "long varchar" | "long" => L::Text { unicode },
            "binary" => L::Binary {
                len: p(0).or(Some(1)),
            },
            "varbinary" => L::Varbinary { len: p(0) },
            "tinyblob" => L::Varbinary { len: Some(255) },
            "blob" | "mediumblob" | "longblob" | "long varbinary" => L::Blob,
            "date" => L::Date,
            "time" => L::Time {
                precision: frac(),
                tz: false,
            },
            "datetime" => L::Timestamp {
                precision: frac(),
                tz: false,
            },
            // TIMESTAMP is stored in UTC and shown in the session's zone.
            "timestamp" => L::Timestamp {
                precision: frac(),
                tz: true,
            },
            "year" => L::Year,
            "json" => L::Json { binary: true },
            // MariaDB 10.7+.
            "uuid" => L::Uuid,
            "inet4" | "inet6" => L::Inet,
            "enum" => L::Enum {
                values: t.args.clone(),
            },
            "set" => L::Set {
                values: t.args.clone(),
            },
            "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
            | "multipolygon" | "geometrycollection" | "geomcollection" => L::Geometry {
                kind: (t.name != "geometry").then(|| {
                    if t.name == "geomcollection" {
                        "geometrycollection".into()
                    } else {
                        t.name.clone()
                    }
                }),
                srid: None,
                geography: false,
            },
            // MySQL 9, MariaDB 11.7, OceanBase: `vector(n)`; TiDB also bare
            // `vector` and `vector<float>`; SingleStore `vector(n, F16)`…
            "vector" => vector_type(t.arg_u32(0), t.args.get(1).map(String::as_str)),
            n if n.starts_with("vector<") => {
                let e = n.trim_start_matches("vector<").trim_end_matches('>');
                vector_type(t.arg_u32(0), Some(e))
            }
            _ => L::Other {
                native: t.raw.clone(),
            },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Vector { sparse: true, .. } => super::vector_fallback(self, t),
            L::Vector { dims, element, .. } => {
                use dbine_driver::VectorElement as E;
                let r = Rendered::exact(match dims {
                    Some(n) => format!("vector({n})"),
                    None => "vector".into(),
                });
                let r = if dims.is_none() {
                    r.with(
                        Warning,
                        TypeApproximated,
                        "Vector sin dimensión: queda «vector» (MySQL toma 2048 dimensiones; MariaDB y OceanBase exigen una).",
                    )
                } else {
                    r
                };
                match element {
                    E::F32 => r,
                    _ => r.with(
                        Warning,
                        TypeApproximated,
                        format!("{}: se guarda como float32.", t.describe()),
                    ),
                }
            }
            L::Bool => Rendered::exact("tinyint(1)"),
            L::Int { bytes, unsigned } => {
                let u = if *unsigned { " unsigned" } else { "" };
                match bytes {
                    1 => Rendered::exact(format!("tinyint{u}")),
                    2 => Rendered::exact(format!("smallint{u}")),
                    3 => Rendered::exact(format!("mediumint{u}")),
                    4 => Rendered::exact(format!("int{u}")),
                    8 => Rendered::exact(format!("bigint{u}")),
                    _ => Rendered::exact(format!("decimal(39, 0){u}")).with(
                        Info,
                        TypeChanged,
                        "Entero de 16 bytes como decimal(39, 0).",
                    ),
                }
            }
            L::Decimal {
                precision: Some(p),
                scale,
            } => {
                let s = scale.unwrap_or(0);
                let (np, ns, lossy) = fit_decimal(*p, s, 65, 30);
                let r = Rendered::exact(format!("decimal({np}, {ns})"));
                if lossy {
                    r.with(Loss, PrecisionLoss, format!("MySQL admite hasta decimal(65, 30); el origen es ({p}, {s}): queda ({np}, {ns})."))
                } else {
                    r
                }
            }
            L::Decimal {
                precision: None, ..
            } => Rendered::exact("decimal(65, 30)").with(
                Loss,
                PrecisionLoss,
                "El origen no fija la precisión: se usa decimal(65, 30), el máximo de MySQL.",
            ),
            L::Float { bytes: 4 } => Rendered::exact("float"),
            L::Float { .. } => Rendered::exact("double"),
            L::Money => Rendered::exact("decimal(19, 4)").with(
                Info,
                TypeChanged,
                "Moneda como decimal(19, 4).",
            ),
            L::Char { len, .. } => {
                let n = len.unwrap_or(1);
                if n <= 255 {
                    Rendered::exact(format!("char({n})"))
                } else if n <= MAX_VARCHAR {
                    Rendered::exact(format!("varchar({n})")).with(
                        Info,
                        TypeChanged,
                        "CHAR de MySQL admite hasta 255: se usa varchar.",
                    )
                } else {
                    Rendered::exact("longtext").with(
                        Info,
                        TypeChanged,
                        format!("char({n}) supera el máximo de MySQL: se usa longtext."),
                    )
                }
            }
            L::Varchar { len: Some(n), .. } if *n <= MAX_VARCHAR => {
                Rendered::exact(format!("varchar({n})"))
            }
            L::Varchar { len: Some(n), .. } => Rendered::exact("longtext").with(
                Info,
                TypeChanged,
                format!("varchar({n}) supera el máximo de MySQL en utf8mb4: se usa longtext."),
            ),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact("longtext"),
            L::Binary { len } => match len.unwrap_or(1) {
                n if n <= 255 => Rendered::exact(format!("binary({n})")),
                n if n <= 65_000 => Rendered::exact(format!("varbinary({n})")).with(
                    Info,
                    TypeChanged,
                    "BINARY de MySQL admite hasta 255: se usa varbinary.",
                ),
                _ => Rendered::exact("longblob"),
            },
            L::Varbinary { len: Some(n) } if *n <= 65_000 => {
                Rendered::exact(format!("varbinary({n})"))
            }
            L::Varbinary { .. } | L::Blob => Rendered::exact("longblob"),
            L::Bit { len } => match len {
                Some(n) if *n <= 64 => Rendered::exact(format!("bit({n})")),
                _ => Rendered::exact("longblob").with(
                    Warning,
                    TypeApproximated,
                    "Cadena de bits de más de 64: se guarda como binario.",
                ),
            },
            L::Date => Rendered::exact("date"),
            L::Time { precision, tz } => {
                let r = Rendered::exact(format!("time{}", fsp(*precision)))
                    .with_loss(precision_loss(*precision, 6));
                if *tz {
                    r.with(
                        Loss,
                        TimeZoneLoss,
                        "MySQL no guarda la zona horaria de una hora.",
                    )
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact(format!("datetime{}", fsp(*precision)))
                    .with_loss(precision_loss(*precision, 6));
                if *tz {
                    r.with(
                        Loss,
                        TimeZoneLoss,
                        "MySQL no guarda la zona horaria: queda como DATETIME. (TIMESTAMP normaliza a UTC pero solo cubre de 1970 a 2038.)",
                    )
                } else {
                    r
                }
            }
            L::Interval => Rendered::exact("varchar(64)").with(
                Loss,
                TypeApproximated,
                "MySQL no tiene intervalos: queda como texto.",
            ),
            L::Year => Rendered::exact("year"),
            L::Uuid => Rendered::exact("char(36)").with(Info, TypeChanged, "UUID como char(36)."),
            L::Json { .. } => Rendered::exact("json"),
            L::Xml => Rendered::exact("longtext").with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } => Rendered::exact(format!(
                "enum({})",
                values
                    .iter()
                    .map(|v| quote(v))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            L::Set { values } if values.len() <= 64 => Rendered::exact(format!(
                "set({})",
                values
                    .iter()
                    .map(|v| quote(v))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            L::Set { values } => Rendered::exact(format!("varchar({})", set_len(values))).with(
                Warning,
                TypeApproximated,
                "SET de MySQL admite hasta 64 valores: queda como texto.",
            ),
            L::Array { .. } | L::Map { .. } => Rendered::exact("json").with(
                Warning,
                TypeApproximated,
                "MySQL no tiene arreglos ni mapas: se guarda como JSON.",
            ),
            L::Geometry { kind, srid, .. } => {
                let r = Rendered::exact(kind.clone().unwrap_or_else(|| "geometry".into()));
                if srid.is_some() {
                    r.with(Info, TypeChanged, "El SRID no se fija en la columna.")
                } else {
                    r
                }
            }
            L::Inet => Rendered::exact("varchar(45)").with(
                Info,
                TypeApproximated,
                "Dirección IP como texto.",
            ),
            L::MacAddr => Rendered::exact("varchar(17)").with(
                Info,
                TypeApproximated,
                "Dirección MAC como texto.",
            ),
            L::RowVersion => Rendered::exact("binary(8)").with(
                Warning,
                TypeApproximated,
                "MySQL no tiene versión de fila automática: no se actualiza sola.",
            ),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        // TEXT, BLOB and JSON take defaults only as expressions (8.0.13+).
        let expr_only = is_lob(ty);
        let v = match d {
            // Only DATETIME and TIMESTAMP take CURRENT_TIMESTAMP as is, and
            // with the column's own fractional precision.
            DefaultValue::CurrentTimestamp => match ty {
                L::Timestamp {
                    precision: Some(0), ..
                } => "CURRENT_TIMESTAMP".into(),
                L::Timestamp { precision, .. } => {
                    format!("CURRENT_TIMESTAMP({})", precision.unwrap_or(6).min(6))
                }
                L::Date => "(CURRENT_DATE)".into(),
                L::Time { .. } => "(CURRENT_TIME)".into(),
                _ => "(CURRENT_TIMESTAMP)".into(),
            },
            DefaultValue::CurrentDate => "(CURRENT_DATE)".into(),
            DefaultValue::CurrentTime => "(CURRENT_TIME)".into(),
            DefaultValue::NewUuid => match uuid_slot(ty) {
                UuidSlot::Native | UuidSlot::Text => "(UUID())".into(),
                // UUID_TO_BIN is MySQL-only; this works on MariaDB too.
                UuidSlot::Binary => "(UNHEX(REPLACE(UUID(), '-', '')))".into(),
                UuidSlot::None => return None,
            },
            // String literals read backslash escapes.
            DefaultValue::Text(s) => quote(&s.replace('\\', "\\\\")),
            other => standard_default(other, ty, "CURRENT_TIMESTAMP", None, true)?,
        };
        Some(if expr_only && !v.starts_with('(') && v != "NULL" {
            format!("({v})")
        } else {
            v
        })
    }

    /// The family's vector indexes (MariaDB, TiDB, OceanBase, SingleStore;
    /// MySQL Community has none, and its DDL says so): the standard metric
    /// and the standard build parameters, each engine's DDL writing them in
    /// its own terms.
    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
        column: Option<&crate::logical::LogicalType>,
    ) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
        // Every engine of the family indexes dense vectors of a fixed
        // dimension (TiDB's `vector` without one takes no index).
        match column {
            Some(L::Vector { sparse: true, .. }) => {
                return Err(
                    "MariaDB, TiDB, OceanBase y SingleStore no indexan vectores dispersos".into(),
                )
            }
            Some(L::Vector { .. }) if super::starrocks::searched_dims(v, column).is_none() => {
                return Err("los índices vectoriales de MariaDB, TiDB, OceanBase y SingleStore necesitan la dimensión de los vectores y no se conoce".into())
            }
            _ => {}
        }
        if let Some(m) = v.metric.as_deref() {
            if !VECTOR_METRICS.contains(&m) {
                return Err(format!(
                    "la distancia «{m}» no está en los índices vectoriales de MySQL, MariaDB, TiDB, OceanBase ni SingleStore"
                ));
            }
        }
        // And the dimension the index holds (Manticore's knn_dims,
        // Doris' dim) or its column's: finalize gives it to the column.
        let params = super::starrocks::index_params(
            v,
            &[
                dbine_driver::vector::param::M,
                dbine_driver::vector::param::EF_CONSTRUCTION,
                dbine_driver::vector::param::LISTS,
            ],
            column,
        );
        Ok((
            Some("VECTOR".into()),
            dbine_driver::VectorIndex {
                algorithm: v.algorithm.clone(),
                metric: v.metric.clone(),
                params,
            },
        ))
    }

    fn caps(&self) -> Caps {
        const ACTIONS: &[&str] = &["CASCADE", "SET NULL", "RESTRICT", "NO ACTION"];
        Caps {
            foreign_keys: true,
            on_delete: ACTIONS,
            on_update: ACTIONS,
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 64,
            case: IdentCase::Preserve,
            vector_indexes: true,
        }
    }

    /// What CREATE TABLE refuses otherwise: one AUTO_INCREMENT column, of an
    /// integer type and first in some key; rows under 65 535 bytes; keys
    /// under 3072 bytes and without TEXT/BLOB columns in full.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        // A vector index's column is a vector(n), its dimension from the
        // index when its type had none; an index on another type is left out.
        super::starrocks::settle_vectors(
            t,
            report,
            &super::starrocks::VectorTarget {
                engine: "el destino",
                ty: &|n| n.map_or_else(|| "vector".into(), |n| format!("vector({n})")),
                is_vector: &|ty| match self.parse_type(&parse(ty)) {
                    L::Vector { dims, .. } => Some(dims),
                    _ => None,
                },
                dim_param: None,
                // MySQL and TiDB take a vector without one.
                needs_dim: false,
                fallback: None,
            },
        );
        single_auto_increment(t, report, "MySQL");
        let table = t.name.clone();

        // AUTO_INCREMENT: integer type, first column of an index.
        for i in 0..t.columns.len() {
            if !t.columns[i].auto_increment {
                continue;
            }
            let c = &mut t.columns[i];
            if !matches!(
                self.parse_type(&parse(&c.data_type)),
                L::Int { bytes: 1..=8, .. }
            ) {
                report.push(
                    Severity::Loss,
                    IssueCode::RangeLoss,
                    &table,
                    Some(&c.name),
                    format!(
                        "AUTO_INCREMENT de MySQL necesita un tipo entero: «{}» pasa a bigint.",
                        c.data_type
                    ),
                );
                c.data_type = "bigint".into();
            }
            let name = c.name.clone();
            let first_of_key = t
                .primary_key
                .as_ref()
                .is_some_and(|k| k.columns.first() == Some(&name))
                || t.indexes.iter().any(|ix| ix.columns.first() == Some(&name));
            if !first_of_key {
                let ix_name: String = format!("{table}_{name}_ai").chars().take(64).collect();
                t.indexes.push(IndexDef {
                    name: ix_name,
                    columns: vec![name.clone()],
                    ..Default::default()
                });
                report.push(
                    Severity::Info,
                    IssueCode::IndexChanged,
                    &table,
                    Some(&name),
                    "MySQL exige que la columna AUTO_INCREMENT encabece un índice: se agrega uno.",
                );
            }
        }

        // Row size: the widest VARCHARs become TEXT until the row fits.
        let null_bytes = (t.columns.len() as u64).div_ceil(8);
        loop {
            let logical: Vec<L> = t
                .columns
                .iter()
                .map(|c| self.parse_type(&parse(&c.data_type)))
                .collect();
            let total: u64 = logical.iter().map(row_bytes).sum::<u64>() + null_bytes;
            if total <= MAX_ROW {
                break;
            }
            let widest = logical
                .iter()
                .enumerate()
                .filter_map(|(i, l)| match l {
                    L::Varchar { len: Some(n), .. } | L::Char { len: Some(n), .. } => Some((i, *n)),
                    L::Varbinary { len: Some(n) } | L::Binary { len: Some(n) } => Some((i, *n / 4)),
                    _ => None,
                })
                .max_by_key(|(i, n)| (*n, std::cmp::Reverse(*i)));
            let Some((i, _)) = widest else { break };
            let c = &mut t.columns[i];
            let binary = matches!(logical[i], L::Varbinary { .. } | L::Binary { .. });
            let to = if binary { "mediumblob" } else { "mediumtext" };
            report.push(
                Severity::Info,
                IssueCode::TypeChanged,
                &table,
                Some(&c.name),
                format!(
                    "La fila supera los 65 535 bytes que admite MySQL: «{}» pasa a {to}.",
                    c.data_type
                ),
            );
            c.data_type = to.into();
            if let Some(d) = c
                .default_value
                .as_mut()
                .filter(|d| !d.starts_with('(') && *d != "NULL")
            {
                *d = format!("({d})");
            }
        }

        // Keys: a TEXT/BLOB column (or one too wide) can't be in a key in full.
        let logical_of = |t: &TableSchema, name: &str| {
            t.columns
                .iter()
                .find(|c| c.name == name)
                .map(|c| self.parse_type(&parse(&c.data_type)))
        };
        if let Some(pk) = t.primary_key.clone() {
            let budget = MAX_KEY / pk.columns.len().max(1) as u64;
            for name in &pk.columns {
                let Some(l) = logical_of(t, name) else {
                    continue;
                };
                let chars = budget / 4;
                let narrow = match l {
                    L::Text { .. } => Some(format!("varchar({chars})")),
                    L::Varchar { len: Some(n), .. } | L::Char { len: Some(n), .. }
                        if u64::from(n) > chars =>
                    {
                        Some(format!("varchar({chars})"))
                    }
                    L::Blob => Some(format!("varbinary({budget})")),
                    L::Varbinary { len: Some(n) } if u64::from(n) > budget => {
                        Some(format!("varbinary({budget})"))
                    }
                    _ => None,
                };
                if let (Some(to), Some(c)) =
                    (narrow, t.columns.iter_mut().find(|c| &c.name == name))
                {
                    report.push(
                        Severity::Loss,
                        IssueCode::LengthLoss,
                        &table,
                        Some(name),
                        format!("La clave primaria de MySQL admite hasta {MAX_KEY} bytes: «{}» pasa a {to}.", c.data_type),
                    );
                    c.data_type = to;
                    if let Some(d) = c
                        .default_value
                        .as_mut()
                        .filter(|d| d.starts_with("('") && d.ends_with("')"))
                    {
                        *d = d[1..d.len() - 1].to_string();
                    }
                }
            }
        }
        let columns = t.columns.clone();
        let mut dropped = Vec::new();
        for (k, ix) in t.indexes.iter_mut().enumerate() {
            let budget = MAX_KEY / ix.columns.len().max(1) as u64;
            for col in ix.columns.iter_mut() {
                let Some(c) = columns.iter().find(|c| &c.name == col) else {
                    continue;
                };
                let l = self.parse_type(&parse(&c.data_type));
                let prefix = match l {
                    L::Json { .. } => {
                        dropped.push(k);
                        None
                    }
                    L::Text { .. } => Some(budget / 4),
                    L::Varchar { len: Some(n), .. } if u64::from(n) * 4 > budget => {
                        Some(budget / 4)
                    }
                    L::Blob => Some(budget),
                    L::Varbinary { len: Some(n) } if u64::from(n) > budget => Some(budget),
                    _ => None,
                };
                if let Some(n) = prefix {
                    report.push(
                        if ix.unique { Severity::Warning } else { Severity::Info },
                        IssueCode::IndexChanged,
                        &table,
                        Some(&ix.name),
                        if ix.unique {
                            format!("MySQL indexa «{col}» por sus primeros {n} caracteres: la unicidad se controla solo sobre ese prefijo.")
                        } else {
                            format!("MySQL indexa «{col}» por sus primeros {n} caracteres.")
                        },
                    );
                    *col = format!("{col}({n})");
                }
            }
        }
        for k in dropped.into_iter().rev() {
            let ix = t.indexes.remove(k);
            report.push(
                Severity::Dropped,
                IssueCode::IndexDropped,
                &table,
                Some(&ix.name),
                "MySQL no indexa columnas JSON: se omite el índice.",
            );
        }
    }
}

/// Fractional seconds of TIME / DATETIME. MySQL's default is 0, so an
/// unspecified precision (PostgreSQL's `timestamp` keeps microseconds)
/// gets 6.
fn fsp(p: Option<u8>) -> String {
    match p {
        Some(0) => String::new(),
        p => format!("({})", p.unwrap_or(6).min(6)),
    }
}

/// Types whose default must be an expression in parentheses.
fn is_lob(ty: &L) -> bool {
    matches!(
        ty,
        L::Text { .. }
            | L::Blob
            | L::Json { .. }
            | L::Varchar { len: None, .. }
            | L::Array { .. }
            | L::Map { .. }
            | L::Xml
            | L::Geometry { .. }
    )
}

/// Bytes a column takes in MySQL's 65 535-byte row (utf8mb4).
fn row_bytes(l: &L) -> u64 {
    match l {
        L::Varchar { len: Some(n), .. } => u64::from(*n) * 4 + 2,
        L::Char { len, .. } => u64::from(len.unwrap_or(1)) * 4,
        L::Binary { len } => u64::from(len.unwrap_or(1)),
        L::Varbinary { len: Some(n) } => u64::from(*n) + 2,
        L::Decimal { precision, .. } => u64::from(precision.unwrap_or(10)) / 2 + 1,
        L::Int { bytes, .. } => u64::from(*bytes),
        L::Bool | L::Year => 1,
        L::Date => 3,
        L::Time { .. } => 6,
        L::Float { bytes } => u64::from(*bytes),
        L::Enum { .. } => 2,
        _ => 12,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, IndexDef, KeyDef};

    fn ty(s: &str) -> L {
        MySql.parse_type(&parse(s))
    }

    #[test]
    fn vectors_parse_render_and_index() {
        use dbine_driver::{VectorElement as E, VectorIndex};
        let v3 = L::Vector {
            dims: Some(3),
            element: E::F32,
            sparse: false,
        };
        assert_eq!(ty("vector(3)"), v3);
        assert_eq!(ty("VECTOR(3)"), v3);
        assert_eq!(ty("vector<float>(3)"), v3);
        assert_eq!(
            ty("vector"),
            L::Vector {
                dims: None,
                element: E::F32,
                sparse: false
            }
        );
        assert_eq!(
            ty("vector(8, F16)"),
            L::Vector {
                dims: Some(8),
                element: E::F16,
                sparse: false
            }
        );
        assert_eq!(MySql.render_type(&v3), Rendered::exact("vector(3)"));
        // Round trip.
        assert_eq!(ty(&MySql.render_type(&v3).native), v3);
        let half = MySql.render_type(&L::Vector {
            dims: Some(4),
            element: E::F16,
            sparse: false,
        });
        assert_eq!(half.native, "vector(4)");
        assert_eq!(half.notes.len(), 1);
        // Sparse vectors: no type for them, a list with a warning.
        let sparse = MySql.render_type(&L::Vector {
            dims: Some(9),
            element: E::F32,
            sparse: true,
        });
        assert_eq!(sparse.notes[0].code, IssueCode::TypeApproximated);
        // A pgvector HNSW index (cosine, m, ef_construction) in this family's terms.
        let pg = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("cosine".into()),
            params: [("m", "16"), ("ef_construction", "64"), ("ef_search", "40")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let (kind, v) = MySql.vector_index(&pg, None).unwrap();
        assert_eq!(kind.as_deref(), Some("VECTOR"));
        assert_eq!(v.metric.as_deref(), Some("cosine"));
        assert_eq!(v.params.len(), 2);
        assert!(MySql
            .vector_index(
                &VectorIndex {
                    metric: Some("hamming".into()),
                    ..Default::default()
                },
                None
            )
            .is_err());
        // The searched column: its dimension rides to finalize; one of
        // unknown dimension, or a sparse one, takes no index.
        let (_, v) = MySql.vector_index(&pg, Some(&v3)).unwrap();
        assert_eq!(v.params.get("dim").map(String::as_str), Some("3"));
        let any = L::Vector {
            dims: None,
            element: E::F32,
            sparse: false,
        };
        let e = MySql.vector_index(&pg, Some(&any)).unwrap_err();
        assert!(e.contains("necesitan la dimensión"), "{e}");
        let mut with_dim = pg.clone();
        with_dim.params.insert("knn_dims".into(), "3".into());
        assert!(MySql.vector_index(&with_dim, Some(&any)).is_ok());
        let sparse = L::Vector {
            dims: Some(9),
            element: E::F32,
            sparse: true,
        };
        let e = MySql.vector_index(&pg, Some(&sparse)).unwrap_err();
        assert!(e.contains("dispersos"), "{e}");
    }

    /// A searched list of numbers whose index states its dimension moves as
    /// a vector out of Doris and StarRocks (`convert`), also to families
    /// outside this one; Manticore's float_vector too, whose dimension the
    /// driver repeats as the index's `dims` option (`knn_dims` is its own
    /// name, which `convert` doesn't read).
    #[test]
    fn olap_vectors_leave_as_vectors() {
        use crate::convert::{convert, Options};
        for from in ["doris", "starrocks", "manticore"] {
            for (to, want) in [
                ("postgres", "vector(3)"),
                ("clickhouse", "Array(Float32)"),
                ("mariadb", "vector(3)"),
            ] {
                let c = convert(&[dim_in_index(from)], from, to, &Options::default()).unwrap();
                let t = &c.tables[0];
                let v = t.columns.iter().find(|c| c.name == "v").unwrap();
                assert_eq!(v.data_type, want, "{from} -> {to}: {:?}", c.issues);
                assert!(
                    t.indexes.iter().any(|i| i.vector.is_some()),
                    "{from} -> {to}: {:?}",
                    c.issues
                );
            }
        }
        // A sparse vector's index isn't taken by the family's engines.
        let mut t = vector_table();
        t.columns[1].data_type = "sparsevec(30000)".into();
        let c = convert(&[t], "postgres", "mariadb", &Options::default()).unwrap();
        assert!(c.tables[0].indexes.iter().all(|i| i.vector.is_none()));
        assert!(c
            .issues
            .iter()
            .any(|i| i.code == IssueCode::IndexDropped && i.message.contains("dispersos")));
    }

    /// A table with a vector column and a vector index, as the MySQL family
    /// describes it.
    fn vector_table() -> TableSchema {
        TableSchema {
            name: "items".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnDef {
                    name: "v".into(),
                    data_type: "vector(3)".into(),
                    nullable: true,
                    ..Default::default()
                },
            ],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into()],
            }),
            indexes: vec![IndexDef {
                name: "iv".into(),
                columns: vec!["v".into()],
                kind: Some("VECTOR".into()),
                vector: Some(dbine_driver::VectorIndex {
                    algorithm: None,
                    metric: Some("l2".into()),
                    params: [("m".to_string(), "8".to_string())].into(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A source table whose vectors' dimension is in their index: Manticore's
    /// float_vector (knn_dims) or Doris' ARRAY<FLOAT> (the ANN index's dim).
    fn dim_in_index(engine: &str) -> TableSchema {
        let (ty, kind, key) = match engine {
            "manticore" => ("float_vector", "KNN", "knn_dims"),
            _ => ("ARRAY<FLOAT>", "ANN", "dim"),
        };
        // Manticore's KNN settings as the driver reads them back: the
        // native ones, and the dimension under `dims`.
        let options = match engine {
            "manticore" => [
                ("knn_type", "hnsw"),
                ("knn_dims", "3"),
                ("hnsw_similarity", "L2"),
                ("dims", "3"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            _ => Default::default(),
        };
        TableSchema {
            name: "items".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: "bigint".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnDef {
                    name: "v".into(),
                    data_type: ty.into(),
                    nullable: false,
                    ..Default::default()
                },
            ],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into()],
            }),
            indexes: vec![IndexDef {
                name: "v".into(),
                columns: vec!["v".into()],
                kind: Some(kind.into()),
                options,
                vector: Some(dbine_driver::VectorIndex {
                    algorithm: Some("hnsw".into()),
                    metric: Some("l2".into()),
                    params: [(key.to_string(), "3".to_string())].into(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn vectors_take_the_dimension_their_index_holds() {
        use crate::convert::{convert, Options};
        for from in ["manticore", "doris", "starrocks"] {
            for (to, want) in [
                ("mariadb", "vector(3)"),
                ("oceanbase", "vector(3)"),
                ("mysql", "vector(3)"),
                ("greptimedb", "VECTOR(3)"),
                ("databend", "VECTOR(3)"),
                ("manticore", "float_vector"),
                ("doris", "ARRAY<FLOAT>"),
                ("starrocks", "ARRAY<FLOAT>"),
            ] {
                if from == to {
                    continue;
                }
                let c = convert(&[dim_in_index(from)], from, to, &Options::default()).unwrap();
                let t = &c.tables[0];
                let v = t.columns.iter().find(|c| c.name == "v").unwrap();
                assert_eq!(v.data_type, want, "{from} -> {to}: {:?}", c.issues);
                let ix = t
                    .indexes
                    .iter()
                    .find(|i| i.vector.is_some())
                    .unwrap_or_else(|| panic!("{from} -> {to}: {:?}", c.issues));
                let p = &ix.vector.as_ref().unwrap().params;
                match to {
                    "manticore" => assert_eq!(p.get("knn_dims").map(String::as_str), Some("3")),
                    "doris" | "starrocks" => {
                        assert_eq!(p.get("dim").map(String::as_str), Some("3"))
                    }
                    // The type holds it: no hint left in the index.
                    _ => assert!(
                        !p.contains_key("dim") && !p.contains_key("knn_dims"),
                        "{p:?}"
                    ),
                }
                // No stale note says the vector has no dimension.
                assert!(
                    !c.issues.iter().any(|i| i.message.contains("sin dimensión")
                        || i.message.contains("sin la dimensión")),
                    "{from} -> {to}: {:?}",
                    c.issues
                );
            }
        }
    }

    /// A Manticore table as its driver describes it converts to another
    /// family with its vectors' dimension (`float_vector` doesn't say it;
    /// the KNN index does). Live: `DBINE_TEST_MANTICORE_URL`
    /// (`mysql://127.0.0.1:port`), skipped without it; the table is dropped
    /// first and at the end.
    #[tokio::test]
    #[ignore]
    async fn manticore_described_converts_with_its_dimension() {
        use crate::convert::{convert, Options};
        let Ok(url) = std::env::var("DBINE_TEST_MANTICORE_URL") else {
            eprintln!("DBINE_TEST_MANTICORE_URL not set; skipping");
            return;
        };
        let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
        let (host, port) = rest.rsplit_once(':').expect("host:port");
        let cfg = dbine_driver::ConnectionConfig {
            driver: "manticore".into(),
            host: host.into(),
            port: port.parse().expect("port"),
            ..Default::default()
        };
        let d = dbine_drivers::find("manticore").expect("manticore driver");
        let mut s = d.connect(&cfg, None).await.expect("connect");
        let drop = "DROP TABLE IF EXISTS dbine_conv_items";
        let mut out = dbine_driver::QueryOutcome::default();
        let _ = s.execute(drop, 10, &mut out).await;
        let mut out = dbine_driver::QueryOutcome::default();
        s.execute(
            "CREATE TABLE dbine_conv_items (title text, v float_vector knn_type='hnsw' knn_dims='3' hnsw_similarity='L2')",
            10,
            &mut out,
        )
        .await
        .expect("create");
        let described = s.database_schema().await;
        let mut out = dbine_driver::QueryOutcome::default();
        let _ = s.execute(drop, 10, &mut out).await;
        let t = described
            .expect("database_schema")
            .into_iter()
            .find(|t| t.name == "dbine_conv_items")
            .expect("described");
        for (to, want) in [("postgres", "vector(3)"), ("clickhouse", "Array(Float32)")] {
            let c = convert(
                std::slice::from_ref(&t),
                "manticore",
                to,
                &Options::default(),
            )
            .unwrap();
            let t = &c.tables[0];
            let v = t.columns.iter().find(|c| c.name == "v").unwrap();
            assert_eq!(v.data_type, want, "{to}: {:?}", c.issues);
            assert!(
                t.indexes.iter().any(|i| i.vector.is_some()),
                "{to}: {:?}",
                c.issues
            );
        }
    }

    #[test]
    fn vector_indexes_on_columns_that_arent_vectors_are_left_out() {
        use crate::convert::{convert, Options};
        // A vector index whose column isn't a vector and that says no
        // dimension (a float array).
        let mut t = dim_in_index("doris");
        t.indexes[0].vector.as_mut().unwrap().params.clear();
        for to in [
            "mariadb",
            "greptimedb",
            "databend",
            "manticore",
            "starrocks",
        ] {
            let c = convert(std::slice::from_ref(&t), "doris", to, &Options::default()).unwrap();
            assert!(
                c.tables[0].indexes.iter().all(|i| i.vector.is_none()),
                "{to}: {:?}",
                c.tables[0].indexes
            );
            assert!(
                c.issues.iter().any(|i| i.code == IssueCode::IndexDropped),
                "{to}: {:?}",
                c.issues
            );
            // The column stays what it was converted to: no vector type.
            let v = c.tables[0].columns.iter().find(|c| c.name == "v").unwrap();
            assert!(
                !v.data_type.to_ascii_lowercase().starts_with("vector"),
                "{to}: {}",
                v.data_type
            );
        }
        // A vector without a dimension and no index to say it: GreptimeDB and
        // Databend fall back to a list, MySQL keeps `vector`.
        let mut m = dim_in_index("manticore");
        m.indexes.clear();
        let g = convert(
            std::slice::from_ref(&m),
            "manticore",
            "greptimedb",
            &Options::default(),
        )
        .unwrap();
        let v = g.tables[0].columns.iter().find(|c| c.name == "v").unwrap();
        assert_eq!(v.data_type, "JSON");
        assert!(g
            .issues
            .iter()
            .any(|i| i.code == IssueCode::TypeApproximated));
        let d = convert(
            std::slice::from_ref(&m),
            "manticore",
            "databend",
            &Options::default(),
        )
        .unwrap();
        let v = d.tables[0].columns.iter().find(|c| c.name == "v").unwrap();
        assert!(v.data_type.starts_with("ARRAY("), "{}", v.data_type);
        let my = convert(&[m], "manticore", "mysql", &Options::default()).unwrap();
        let v = my.tables[0].columns.iter().find(|c| c.name == "v").unwrap();
        assert_eq!(v.data_type, "vector");
    }

    /// Doris (and StarRocks, with a key PRIMARY KEY can't hold) take a
    /// vector index only on DUPLICATE KEY tables, but only an index that is
    /// created costs the table its key: one with filter columns, or on a
    /// vector of unknown dimension, is left out and the key stays.
    #[test]
    fn a_vector_index_left_out_keeps_the_olap_key() {
        use crate::convert::{convert, Options};
        // A filter column (pgvector-like source) and a vector of unknown
        // dimension (TiDB's `vector`, same family).
        let mut filtered = vector_table();
        filtered.columns.push(ColumnDef {
            name: "cat".into(),
            data_type: "int".into(),
            nullable: true,
            ..Default::default()
        });
        filtered.indexes[0].columns.push("cat".into());
        let mut no_dim = vector_table();
        no_dim.columns[1].data_type = "vector".into();
        let mut decimal_key = no_dim.clone();
        decimal_key.columns[0].data_type = "decimal(10,2)".into();
        for (t, from, to, model) in [
            (&filtered, "mariadb", "doris", "unique"),
            (&no_dim, "tidb", "doris", "unique"),
            (&decimal_key, "tidb", "starrocks", "unique"),
        ] {
            let c = convert(std::slice::from_ref(t), from, to, &Options::default()).unwrap();
            let t = &c.tables[0];
            assert!(
                t.indexes.iter().all(|i| i.vector.is_none()),
                "{from} -> {to}"
            );
            assert!(
                c.issues.iter().any(|i| i.code == IssueCode::IndexDropped),
                "{from} -> {to}: {:?}",
                c.issues
            );
            assert_eq!(
                t.options.get("key_model").map(String::as_str),
                Some(model),
                "{from} -> {to}: {:?}",
                c.issues
            );
            assert!(t.primary_key.is_some(), "{from} -> {to}");
            assert!(
                !c.issues
                    .iter()
                    .any(|i| i.code == IssueCode::PrimaryKeyDropped),
                "{from} -> {to}: {:?}",
                c.issues
            );
        }
    }

    #[test]
    fn vectors_cross_into_the_family_engines() {
        use crate::convert::{convert, Options};
        let conv =
            |to: &str| convert(&[vector_table()], "mariadb", to, &Options::default()).unwrap();
        let ix_of = |t: &TableSchema| t.indexes.iter().find(|i| i.vector.is_some()).cloned();
        // GreptimeDB and Databend: VECTOR(3) and their vector index.
        for to in ["greptimedb", "databend"] {
            let c = conv(to);
            let t = &c.tables[0];
            let v = t.columns.iter().find(|c| c.name == "v").unwrap();
            assert_eq!(v.data_type, "VECTOR(3)", "{to}");
            let ix = ix_of(t).unwrap_or_else(|| panic!("{to}: {:?}", c.issues));
            assert_eq!(ix.vector.unwrap().metric.as_deref(), Some("l2"), "{to}");
        }
        // StarRocks and Doris: ARRAY<FLOAT> NOT NULL, the dimension in the index.
        for to in ["starrocks", "doris"] {
            let c = conv(to);
            let t = &c.tables[0];
            // Doris' ANN index takes DUPLICATE KEY tables only: said.
            if to == "doris" {
                assert_eq!(
                    t.options.get("key_model").map(String::as_str),
                    Some("duplicate")
                );
                assert!(c
                    .issues
                    .iter()
                    .any(|i| i.code == IssueCode::PrimaryKeyDropped
                        && i.message.contains("DUPLICATE KEY")));
            }
            let v = t.columns.iter().find(|c| c.name == "v").unwrap();
            assert_eq!(v.data_type, "ARRAY<FLOAT>", "{to}");
            assert!(!v.nullable, "{to}");
            let ix = ix_of(t).unwrap_or_else(|| panic!("{to}: {:?}", c.issues));
            let vi = ix.vector.unwrap();
            assert_eq!(vi.params.get("dim").map(String::as_str), Some("3"), "{to}");
            assert_eq!(vi.algorithm.as_deref(), Some("hnsw"));
        }
        // Manticore: float_vector with its KNN settings, named after the column.
        let c = conv("manticore");
        let t = &c.tables[0];
        assert_eq!(
            t.columns.iter().find(|c| c.name == "v").unwrap().data_type,
            "float_vector"
        );
        let ix = ix_of(t).unwrap();
        assert_eq!(ix.name, "v");
        assert_eq!(
            ix.vector
                .unwrap()
                .params
                .get("knn_dims")
                .map(String::as_str),
            Some("3")
        );
        // And back: GreptimeDB's vector into MySQL's family.
        let back = convert(
            &conv("greptimedb").tables,
            "greptimedb",
            "tidb",
            &Options::default(),
        )
        .unwrap();
        let t = &back.tables[0];
        assert_eq!(
            t.columns.iter().find(|c| c.name == "v").unwrap().data_type,
            "vector(3)"
        );
        let ix = ix_of(t).unwrap();
        assert_eq!(ix.kind.as_deref(), Some("VECTOR"));
        assert_eq!(ix.vector.unwrap().metric.as_deref(), Some("l2"));
        // A distance none of them has is said and left out.
        let mut odd = vector_table();
        odd.indexes[0].vector.as_mut().unwrap().metric = Some("hamming".into());
        let c = convert(&[odd], "mariadb", "greptimedb", &Options::default()).unwrap();
        assert!(c.tables[0].indexes.is_empty());
        assert!(c.issues.iter().any(|i| i.code == IssueCode::IndexDropped));
    }

    #[test]
    fn parses_column_type_spellings() {
        assert_eq!(ty("int(11)"), L::int(4));
        assert_eq!(ty("int"), L::int(4));
        assert_eq!(
            ty("int(10) unsigned"),
            L::Int {
                bytes: 4,
                unsigned: true
            }
        );
        assert_eq!(
            ty("bigint(20) unsigned zerofill"),
            L::Int {
                bytes: 8,
                unsigned: true
            }
        );
        assert_eq!(ty("tinyint(1)"), L::Bool);
        assert_eq!(
            ty("tinyint(1) unsigned"),
            L::Int {
                bytes: 1,
                unsigned: true
            }
        );
        assert_eq!(ty("tinyint(4)"), L::int(1));
        assert_eq!(ty("bit(1)"), L::Bool);
        assert_eq!(ty("bit(8)"), L::Bit { len: Some(8) });
        assert_eq!(
            ty("decimal(65,30)"),
            L::Decimal {
                precision: Some(65),
                scale: Some(30)
            }
        );
        assert_eq!(
            ty("decimal(10,0) unsigned"),
            L::Decimal {
                precision: Some(10),
                scale: Some(0)
            }
        );
        assert_eq!(ty("float"), L::Float { bytes: 4 });
        assert_eq!(ty("float(7,2)"), L::Float { bytes: 4 });
        assert_eq!(ty("float(30)"), L::Float { bytes: 8 });
        assert_eq!(ty("double"), L::Float { bytes: 8 });
        assert_eq!(
            ty("datetime(6)"),
            L::Timestamp {
                precision: Some(6),
                tz: false
            }
        );
        assert_eq!(
            ty("timestamp(3)"),
            L::Timestamp {
                precision: Some(3),
                tz: true
            }
        );
        assert_eq!(
            ty("time(6)"),
            L::Time {
                precision: Some(6),
                tz: false
            }
        );
        assert_eq!(ty("year(4)"), L::Year);
        assert_eq!(
            ty("enum('rojo','verde','it''s')"),
            L::Enum {
                values: vec!["rojo".into(), "verde".into(), "it's".into()]
            }
        );
        assert_eq!(
            ty("set('a','b')"),
            L::Set {
                values: vec!["a".into(), "b".into()]
            }
        );
        assert_eq!(ty("uuid"), L::Uuid);
        assert_eq!(ty("inet6"), L::Inet);
        assert_eq!(ty("longtext"), L::Text { unicode: true });
        assert_eq!(
            ty("geomcollection"),
            L::Geometry {
                kind: Some("geometrycollection".into()),
                srid: None,
                geography: false
            }
        );
        assert_eq!(ty("int GENERATED ALWAYS AS ((`a` + 1)) VIRTUAL"), L::int(4));
    }

    #[test]
    fn decimals_keep_their_integer_digits() {
        let r = MySql.render_type(&L::Decimal {
            precision: Some(40),
            scale: Some(35),
        });
        assert_eq!(r.native, "decimal(35, 30)");
        assert_eq!(r.notes[0].code, IssueCode::PrecisionLoss);
    }

    #[test]
    fn defaults_by_column_type() {
        let ts = DefaultValue::CurrentTimestamp;
        assert_eq!(
            MySql.render_default(&ts, &L::Date).as_deref(),
            Some("(CURRENT_DATE)")
        );
        assert_eq!(
            MySql
                .render_default(
                    &ts,
                    &L::Timestamp {
                        precision: Some(3),
                        tz: false
                    }
                )
                .as_deref(),
            Some("CURRENT_TIMESTAMP(3)")
        );
        assert_eq!(
            MySql
                .render_default(
                    &ts,
                    &L::Timestamp {
                        precision: None,
                        tz: false
                    }
                )
                .as_deref(),
            Some("CURRENT_TIMESTAMP(6)")
        );
        assert_eq!(
            MySql
                .render_type(&L::Timestamp {
                    precision: None,
                    tz: false
                })
                .native,
            "datetime(6)"
        );
        assert_eq!(
            MySql
                .render_type(&L::Timestamp {
                    precision: Some(0),
                    tz: false
                })
                .native,
            "datetime"
        );
        assert_eq!(
            MySql
                .render_default(
                    &DefaultValue::Text("a\\b".into()),
                    &L::Varchar {
                        len: Some(9),
                        unicode: true
                    }
                )
                .as_deref(),
            Some("'a\\\\b'")
        );
        assert_eq!(
            MySql
                .render_default(&DefaultValue::Text("x".into()), &L::Text { unicode: true })
                .as_deref(),
            Some("('x')")
        );
        assert_eq!(
            MySql
                .render_default(&DefaultValue::NewUuid, &L::Varbinary { len: Some(16) })
                .as_deref(),
            Some("(UNHEX(REPLACE(UUID(), '-', '')))")
        );
    }

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            nullable: true,
            ..Default::default()
        }
    }

    #[test]
    fn finalize_fixes_keys_rows_and_auto_increment() {
        let mut t = TableSchema {
            name: "t".into(),
            columns: vec![
                ColumnDef {
                    auto_increment: true,
                    ..col("id", "decimal(39, 0)")
                },
                ColumnDef {
                    auto_increment: true,
                    ..col("otro", "int")
                },
                col("k", "longtext"),
                col("a", "varchar(10000)"),
                col("b", "varchar(10000)"),
                col("j", "json"),
            ],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into()],
            }),
            indexes: vec![
                IndexDef {
                    name: "ix_k".into(),
                    columns: vec!["k".into()],
                    ..Default::default()
                },
                IndexDef {
                    name: "ix_j".into(),
                    columns: vec!["j".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut r = Report::default();
        MySql.finalize(&mut t, &mut r);
        assert_eq!(t.columns[0].data_type, "bigint");
        assert!(!t.columns[1].auto_increment);
        assert_eq!(t.columns[3].data_type, "mediumtext");
        assert_eq!(t.columns[4].data_type, "varchar(10000)");
        assert_eq!(t.indexes[0].columns, vec!["k(768)"]);
        assert_eq!(t.indexes.len(), 1, "JSON index dropped");
    }

    /// The native name and the worst note's severity (None: exact).
    fn render(t: L) -> (String, Option<Severity>) {
        let r = MySql.render_type(&t);
        (r.native, r.notes.iter().map(|n| n.severity).max())
    }

    #[test]
    fn renders_every_type_within_mysqls_limits() {
        use Severity::*;
        let text = |len| L::Varchar { len, unicode: true };
        let fixed = |len| L::Char {
            len,
            unicode: false,
        };
        let cases = [
            (L::Bool, "tinyint(1)", None),
            (
                L::Int {
                    bytes: 3,
                    unsigned: true,
                },
                "mediumint unsigned",
                None,
            ),
            (L::int(16), "decimal(39, 0)", Some(Info)),
            (
                L::Decimal {
                    precision: Some(70),
                    scale: Some(40),
                },
                // The 30 integer digits stay; the scale drops to MySQL's 30.
                "decimal(60, 30)",
                Some(Loss),
            ),
            (
                L::Decimal {
                    precision: Some(12),
                    scale: None,
                },
                "decimal(12, 0)",
                None,
            ),
            (
                L::Decimal {
                    precision: None,
                    scale: None,
                },
                "decimal(65, 30)",
                Some(Loss),
            ),
            (L::Float { bytes: 4 }, "float", None),
            (L::Float { bytes: 8 }, "double", None),
            (L::Money, "decimal(19, 4)", Some(Info)),
            (fixed(None), "char(1)", None),
            (fixed(Some(300)), "varchar(300)", Some(Info)),
            (fixed(Some(20_000)), "longtext", Some(Info)),
            (text(Some(16_383)), "varchar(16383)", None),
            (text(Some(16_384)), "longtext", Some(Info)),
            (text(None), "longtext", None),
            (L::Binary { len: None }, "binary(1)", None),
            (L::Binary { len: Some(1000) }, "varbinary(1000)", Some(Info)),
            (L::Binary { len: Some(70_000) }, "longblob", None),
            (L::Varbinary { len: Some(100) }, "varbinary(100)", None),
            (L::Varbinary { len: Some(70_000) }, "longblob", None),
            (L::Bit { len: Some(64) }, "bit(64)", None),
            (L::Bit { len: Some(65) }, "longblob", Some(Warning)),
            (L::Bit { len: None }, "longblob", Some(Warning)),
            (
                L::Time {
                    precision: Some(3),
                    tz: true,
                },
                "time(3)",
                Some(Loss),
            ),
            (
                L::Timestamp {
                    precision: Some(9),
                    tz: false,
                },
                "datetime(6)",
                Some(Loss),
            ),
            (
                L::Timestamp {
                    precision: None,
                    tz: true,
                },
                // Unknown precision: the most MySQL keeps.
                "datetime(6)",
                Some(Loss),
            ),
            (L::Interval, "varchar(64)", Some(Loss)),
            (L::Year, "year", None),
            (L::Uuid, "char(36)", Some(Info)),
            (L::Json { binary: true }, "json", None),
            (L::Xml, "longtext", Some(Info)),
            (
                L::Enum {
                    values: vec!["a'b".into(), "c".into()],
                },
                "enum('a''b', 'c')",
                None,
            ),
            (
                L::Set {
                    values: vec!["x".into(), "y".into()],
                },
                "set('x', 'y')",
                None,
            ),
            (
                L::Array {
                    of: Box::new(L::int(4)),
                },
                "json",
                Some(Warning),
            ),
            (
                L::Geometry {
                    kind: Some("point".into()),
                    srid: Some(4326),
                    geography: false,
                },
                "point",
                Some(Info),
            ),
            (
                L::Geometry {
                    kind: None,
                    srid: None,
                    geography: false,
                },
                "geometry",
                None,
            ),
            (L::Inet, "varchar(45)", Some(Info)),
            (L::MacAddr, "varchar(17)", Some(Info)),
            (L::RowVersion, "binary(8)", Some(Warning)),
            (
                L::Other {
                    native: "vector(3)".into(),
                },
                "vector(3)",
                None,
            ),
        ];
        for (t, native, worst) in cases {
            assert_eq!(render(t.clone()), (native.to_string(), worst), "{t:?}");
        }
        // More than 64 members: no SET, the longest value as text.
        let many: Vec<String> = (0..65).map(|i| format!("v{i}")).collect();
        let (native, worst) = render(L::Set { values: many });
        assert!(native.starts_with("varchar("), "{native}");
        assert_eq!(worst, Some(Warning));
    }
}
