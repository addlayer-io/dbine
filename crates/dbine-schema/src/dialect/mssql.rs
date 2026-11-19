//! SQL Server and the engines that speak T-SQL types (Azure SQL, Fabric
//! Warehouse, Babelfish).
//!
//! Types arrive as the driver builds them from `sys.columns`:
//! `TYPE_NAME(user_type_id)` plus the length in characters
//! (`nvarchar(50)`, `varchar(max)` for `max_length = -1`), `decimal(18,2)`
//! and the fractional digits of `datetime2(7)`, `time(7)`,
//! `datetimeoffset(7)`; `float` / `real` / `datetime` bare. Computed
//! columns arrive as `AS expr [PERSISTED]`.

use super::postgres::{
    fit_decimal, longest, prec, precision_loss, set_len, single_auto_increment, uuid_slot, UuidSlot,
};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{parse, TypeSpec};
use dbine_driver::TableSchema;

pub struct MsSql;

/// Most dimensions of a `vector`: 1998 of float32, 3996 of float16.
const MAX_VECTOR_DIMS: u32 = 1998;
const MAX_VECTOR_DIMS_F16: u32 = 3996;

/// `vector(n)` / `vector(n, float16)`; a vector SQL Server can't declare
/// (sparse, no dimension, too many) goes as JSON text.
fn render_vector(d: &dyn Dialect, t: &L) -> Rendered {
    use dbine_driver::VectorElement as E;
    use IssueCode::*;
    use Severity::*;
    let L::Vector {
        dims,
        element,
        sparse,
    } = t
    else {
        return d.render_type(t);
    };
    let max = if *element == E::F16 {
        MAX_VECTOR_DIMS_F16
    } else {
        MAX_VECTOR_DIMS
    };
    match dims {
        Some(n) if !*sparse && (1..=max).contains(n) => match element {
            E::F32 => Rendered::exact(format!("vector({n})")),
            E::F16 => Rendered::exact(format!("vector({n}, float16)")).with(
                Info,
                TypeChanged,
                "vector de float16: en SQL Server 2025 necesita PREVIEW_FEATURES = ON.",
            ),
            E::F64 => Rendered::exact(format!("vector({n})")).with(
                Loss,
                PrecisionLoss,
                "SQL Server guarda vectores de float32: los float64 pierden precisión.",
            ),
            E::Bf16 => Rendered::exact(format!("vector({n})")).with(
                Info,
                TypeChanged,
                "Vector de bfloat16 como float32 (sin pérdida).",
            ),
            _ => Rendered::exact(format!("vector({n})")).with(
                Info,
                TypeChanged,
                format!("{}: como vector de float32.", t.describe()),
            ),
        },
        _ => {
            let why = if *sparse {
                "SQL Server no tiene vectores dispersos".to_string()
            } else if dims.is_none() {
                "SQL Server necesita la dimensión del vector".to_string()
            } else {
                format!("SQL Server admite vectores de hasta {max} dimensiones")
            };
            Rendered::exact("nvarchar(max)").with(
                Warning,
                TypeApproximated,
                format!("{}: {why}; queda como JSON en nvarchar(max).", t.describe()),
            )
        }
    }
}

/// Index key limits, in bytes: clustered (the primary key by default) and
/// nonclustered.
const MAX_CLUSTERED_KEY: u32 = 900;
const MAX_KEY: u32 = 1700;

impl Dialect for MsSql {
    fn id(&self) -> &'static str {
        "mssql"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let len = || if t.is_max() { None } else { p(0) };
        let frac = |default: u8| Some(p(0).map_or(default, |x| x.min(255) as u8));
        match t.name.as_str() {
            "bit" => L::Bool,
            // TINYINT is 0–255 in SQL Server.
            "tinyint" => L::Int {
                bytes: 1,
                unsigned: true,
            },
            "smallint" => L::int(2),
            "int" | "integer" => L::int(4),
            "bigint" => L::int(8),
            "decimal" | "numeric" | "dec" => L::Decimal {
                precision: p(0).or(Some(18)),
                scale: p(1).or(Some(0)),
            },
            "money" | "smallmoney" => L::Money,
            "real" => L::Float { bytes: 4 },
            "float" | "double precision" => L::Float {
                bytes: if p(0).is_some_and(|b| b <= 24) { 4 } else { 8 },
            },
            "char" | "character" => L::Char {
                len: p(0).or(Some(1)),
                unicode: false,
            },
            "nchar" | "national char" | "national character" => L::Char {
                len: p(0).or(Some(1)),
                unicode: true,
            },
            "varchar" | "character varying" | "char varying" if t.is_max() => {
                L::Text { unicode: false }
            }
            "varchar" | "character varying" | "char varying" => L::Varchar {
                len: p(0).or(Some(1)),
                unicode: false,
            },
            "nvarchar" | "national character varying" | "national char varying" if t.is_max() => {
                L::Text { unicode: true }
            }
            "nvarchar" | "national character varying" | "national char varying" => L::Varchar {
                len: p(0).or(Some(1)),
                unicode: true,
            },
            // Object names: nvarchar(128).
            "sysname" => L::Varchar {
                len: Some(128),
                unicode: true,
            },
            "text" => L::Text { unicode: false },
            "ntext" => L::Text { unicode: true },
            "binary" => L::Binary {
                len: p(0).or(Some(1)),
            },
            "varbinary" | "binary varying" if t.is_max() => L::Blob,
            "varbinary" | "binary varying" => L::Varbinary {
                len: len().or(Some(1)),
            },
            "image" => L::Blob,
            "date" => L::Date,
            "time" => L::Time {
                precision: frac(7),
                tz: false,
            },
            "datetime" => L::Timestamp {
                precision: Some(3),
                tz: false,
            },
            "smalldatetime" => L::Timestamp {
                precision: Some(0),
                tz: false,
            },
            "datetime2" => L::Timestamp {
                precision: frac(7),
                tz: false,
            },
            "datetimeoffset" => L::Timestamp {
                precision: frac(7),
                tz: true,
            },
            "uniqueidentifier" => L::Uuid,
            "xml" => L::Xml,
            // SQL Server 2025 / Azure SQL.
            "json" => L::Json { binary: true },
            // SQL Server 2025 / Azure SQL: `vector(n[, float32|float16])`.
            "vector" => L::Vector {
                dims: p(0),
                element: match t
                    .args
                    .get(1)
                    .map(|a| a.trim().to_ascii_lowercase())
                    .as_deref()
                {
                    Some("float16") => dbine_driver::VectorElement::F16,
                    _ => dbine_driver::VectorElement::F32,
                },
                sparse: false,
            },
            // In SQL Server `timestamp` is the old name of rowversion.
            "rowversion" | "timestamp" => L::RowVersion,
            "geometry" => L::Geometry {
                kind: None,
                srid: None,
                geography: false,
            },
            "geography" => L::Geometry {
                kind: None,
                srid: None,
                geography: true,
            },
            _ => L::Other {
                native: t.raw.clone(),
            },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Vector { .. } => render_vector(self, t),
            L::Bool => Rendered::exact("bit"),
            L::Int { bytes: 1, unsigned: true } => Rendered::exact("tinyint"),
            L::Int { bytes, unsigned } => match L::signed_bytes_for(*bytes, *unsigned) {
                1 | 2 => Rendered::exact("smallint"),
                3 | 4 => Rendered::exact("int"),
                8 => Rendered::exact("bigint"),
                _ if *bytes == 8 => Rendered::exact("decimal(20, 0)").with(Info, TypeChanged, "Entero de 8 bytes sin signo como decimal(20, 0)."),
                _ => Rendered::exact("decimal(38, 0)").with(Loss, RangeLoss, "Entero de 16 bytes como decimal(38, 0): no entran los valores de 39 dígitos."),
            },
            L::Decimal { precision: Some(p), scale } => {
                let s = scale.unwrap_or(0);
                let (np, ns, lossy) = fit_decimal(*p, s, 38, 38);
                let r = Rendered::exact(format!("decimal({np}, {ns})"));
                if lossy {
                    r.with(Loss, PrecisionLoss, format!("SQL Server admite hasta 38 dígitos; el origen es ({p}, {s}): queda ({np}, {ns})."))
                } else {
                    r
                }
            }
            L::Decimal { precision: None, .. } => Rendered::exact("decimal(38, 10)")
                .with(Loss, PrecisionLoss, "El origen no fija la precisión: se usa decimal(38, 10)."),
            L::Float { bytes: 4 } => Rendered::exact("real"),
            L::Float { .. } => Rendered::exact("float"),
            L::Money => Rendered::exact("money"),
            L::Char { len, unicode } => {
                let n = len.unwrap_or(1);
                let (ty, max) = if *unicode { ("nchar", 4000) } else { ("char", 8000) };
                if n <= max {
                    Rendered::exact(format!("{ty}({n})"))
                } else {
                    Rendered::exact(format!("{}(max)", if *unicode { "nvarchar" } else { "varchar" }))
                        .with(Info, TypeChanged, format!("{ty} admite hasta {max}: se usa un texto variable."))
                }
            }
            L::Varchar { len, unicode } => {
                let (ty, max) = if *unicode { ("nvarchar", 4000) } else { ("varchar", 8000) };
                match len {
                    Some(n) if *n <= max => Rendered::exact(format!("{ty}({n})")),
                    _ => Rendered::exact(format!("{ty}(max)")),
                }
            }
            L::Text { unicode } => Rendered::exact(if *unicode { "nvarchar(max)" } else { "varchar(max)" }),
            L::Binary { len } => match len {
                Some(n) if *n <= 8000 => Rendered::exact(format!("binary({n})")),
                _ => Rendered::exact("varbinary(max)"),
            },
            L::Varbinary { len: Some(n) } if *n <= 8000 => Rendered::exact(format!("varbinary({n})")),
            L::Varbinary { .. } | L::Blob => Rendered::exact("varbinary(max)"),
            L::Bit { len: Some(1) } => Rendered::exact("bit"),
            L::Bit { len } => Rendered::exact(format!("varbinary({})", len.map_or("max".into(), |n| n.div_ceil(8).to_string())))
                .with(Warning, TypeApproximated, "SQL Server no tiene cadenas de bits: se guarda como binario."),
            L::Date => Rendered::exact("date"),
            L::Time { precision, tz } => {
                let r = Rendered::exact(format!("time{}", prec(*precision, 7))).with_loss(precision_loss(*precision, 7));
                if *tz {
                    r.with(Loss, TimeZoneLoss, "SQL Server no guarda la zona horaria de una hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz: false } => Rendered::exact(format!("datetime2{}", prec(*precision, 7))).with_loss(precision_loss(*precision, 7)),
            L::Timestamp { precision, tz: true } => Rendered::exact(format!("datetimeoffset{}", prec(*precision, 7))).with_loss(precision_loss(*precision, 7)),
            L::Interval => Rendered::exact("varchar(64)").with(Loss, TypeApproximated, "SQL Server no tiene intervalos: queda como texto."),
            L::Year => Rendered::exact("smallint").with(Info, TypeChanged, "Año como smallint."),
            L::Uuid => Rendered::exact("uniqueidentifier"),
            L::Json { .. } => Rendered::exact("nvarchar(max)").with(Info, TypeChanged, "JSON como nvarchar(max) (el tipo json solo existe en SQL Server 2025 y Azure)."),
            L::Xml => Rendered::exact("xml"),
            L::Enum { values } => Rendered::exact(format!("nvarchar({})", longest(values).min(4000)))
                .with(Warning, TypeApproximated, format!("SQL Server no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => Rendered::exact(match set_len(values) {
                n if n <= 4000 => format!("nvarchar({n})"),
                _ => "nvarchar(max)".into(),
            })
            .with(Warning, TypeApproximated, format!("SQL Server no tiene conjuntos: queda como texto separado por comas. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } => Rendered::exact("nvarchar(max)").with(Warning, TypeApproximated, "SQL Server no tiene arreglos ni mapas: se guarda como JSON en texto."),
            L::Geometry { geography, .. } => Rendered::exact(if *geography { "geography" } else { "geometry" }),
            L::Inet => Rendered::exact("varchar(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("varchar(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("rowversion"),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp => Some(match ty {
                L::Timestamp { tz: true, .. } => "SYSDATETIMEOFFSET()".into(),
                L::Timestamp {
                    precision: Some(p), ..
                } if *p <= 3 => "GETDATE()".into(),
                L::Date => "CAST(GETDATE() AS date)".into(),
                L::Time { .. } => "CAST(SYSDATETIME() AS time)".into(),
                _ => "SYSDATETIME()".into(),
            }),
            DefaultValue::CurrentDate => Some("CAST(GETDATE() AS date)".into()),
            DefaultValue::CurrentTime => Some("CAST(SYSDATETIME() AS time)".into()),
            DefaultValue::NewUuid => match uuid_slot(ty) {
                UuidSlot::Native => Some("NEWID()".into()),
                UuidSlot::Text => Some("LOWER(CONVERT(char(36), NEWID()))".into()),
                UuidSlot::Binary => Some("CAST(NEWID() AS binary(16))".into()),
                UuidSlot::None => None,
            },
            DefaultValue::Text(s)
                if matches!(
                    ty,
                    L::Char { unicode: true, .. }
                        | L::Varchar { unicode: true, .. }
                        | L::Text { unicode: true }
                ) =>
            {
                Some(format!("N{}", crate::default::quote(s)))
            }
            other => standard_default(other, ty, "SYSDATETIME()", Some("NEWID()"), true),
        }
    }

    fn caps(&self) -> Caps {
        const ACTIONS: &[&str] = &["CASCADE", "SET NULL", "SET DEFAULT", "NO ACTION"];
        Caps {
            foreign_keys: true,
            on_delete: ACTIONS,
            on_update: ACTIONS,
            indexes: true,
            partial_indexes: true,
            supports_include: true,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 128,
            case: IdentCase::Preserve,
            vector_indexes: true,
            vectors: true,
        }
    }

    fn implies_auto_increment(&self, t: &TypeSpec) -> bool {
        t.has("identity")
    }

    /// `CREATE VECTOR INDEX … WITH (METRIC = 'cosine' | 'euclidean' |
    /// 'dot', TYPE = 'DiskANN')`: SQL Server picks DiskANN's parameters.
    /// Its column must be a `vector(n)` here (float32 or float16, both
    /// indexed); one [`render_vector`] writes as JSON text (sparse, without
    /// a dimension, too many) can't be indexed.
    fn vector_index(
        &self,
        v: &dbine_driver::VectorIndex,
        column: Option<&crate::logical::LogicalType>,
    ) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
        use dbine_driver::vector::metric;
        if let Some(c @ L::Vector { .. }) = column {
            if !render_vector(self, c).native.starts_with("vector(") {
                return Err(
                    "su columna no es un vector en SQL Server (es dispersa, no tiene dimensión o tiene demasiadas)"
                        .into(),
                );
            }
        }
        let m = v.metric.as_deref().map(str::to_ascii_lowercase);
        let metric = match m.as_deref() {
            None => None,
            Some(metric::COSINE) => Some(metric::COSINE),
            Some(metric::L2 | "euclidean") => Some(metric::L2),
            Some(metric::IP | "dot" | "inner_product") => Some(metric::IP),
            Some(other) => {
                return Err(format!(
                    "SQL Server no tiene la distancia «{other}» (solo coseno, euclidiana y producto interno)"
                ))
            }
        };
        Ok((
            Some("VECTOR".into()),
            dbine_driver::VectorIndex {
                algorithm: Some("diskann".into()),
                metric: metric.map(str::to_string),
                params: Default::default(),
            },
        ))
    }

    /// What CREATE TABLE refuses otherwise: one IDENTITY of an integer
    /// type, no `(max)` column in a key, no cascading self-reference.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        single_auto_increment(t, report, "SQL Server");
        let table = t.name.clone();
        for c in t.columns.iter_mut().filter(|c| c.auto_increment) {
            let ok = matches!(
                self.parse_type(&parse(&c.data_type)),
                L::Int { .. }
                    | L::Decimal {
                        scale: Some(0) | None,
                        ..
                    }
            );
            if !ok {
                report.push(
                    Severity::Loss,
                    IssueCode::RangeLoss,
                    &table,
                    Some(&c.name),
                    format!(
                        "IDENTITY de SQL Server necesita un tipo entero: «{}» pasa a bigint.",
                        c.data_type
                    ),
                );
                c.data_type = "bigint".into();
            }
        }

        // Key columns can't be (max), text, xml…: sized types within the key limit.
        let mut keys: Vec<(Vec<String>, u32, String)> = Vec::new();
        if let Some(pk) = &t.primary_key {
            keys.push((
                pk.columns.clone(),
                MAX_CLUSTERED_KEY,
                "la clave primaria".into(),
            ));
        }
        for ix in &t.indexes {
            keys.push((
                ix.columns.clone(),
                MAX_KEY,
                format!("el índice «{}»", ix.name),
            ));
        }
        for (cols, limit, what) in keys {
            let budget = limit / cols.len().max(1) as u32;
            for name in &cols {
                let Some(c) = t.columns.iter_mut().find(|c| &c.name == name) else {
                    continue;
                };
                let to = match self.parse_type(&parse(&c.data_type)) {
                    L::Text { unicode: true } | L::Xml | L::Json { .. } => {
                        format!("nvarchar({})", budget / 2)
                    }
                    L::Text { unicode: false } => format!("varchar({budget})"),
                    L::Blob => format!("varbinary({budget})"),
                    _ => continue,
                };
                report.push(
                    Severity::Loss,
                    IssueCode::LengthLoss,
                    &table,
                    Some(name),
                    format!(
                        "SQL Server no admite «{}» en {what} (hasta {limit} bytes): pasa a {to}.",
                        c.data_type
                    ),
                );
                c.data_type = to;
            }
        }

        // A self-reference can't cascade (it would form a cycle).
        for fk in t.foreign_keys.iter_mut().filter(|fk| fk.ref_table == table) {
            for (action, clause) in [
                (&mut fk.on_delete, "ON DELETE"),
                (&mut fk.on_update, "ON UPDATE"),
            ] {
                if action.as_deref().is_some_and(|a| a != "NO ACTION") {
                    report.push(
                        Severity::Warning,
                        IssueCode::ForeignKeyActionChanged,
                        &table,
                        fk.name.as_deref(),
                        format!("SQL Server no admite {clause} {} en una referencia a la misma tabla: queda NO ACTION.", action.as_deref().unwrap_or("")),
                    );
                    *action = None;
                }
            }
        }
    }
}

/// T-SQL engines without vectors (Fabric Warehouse, Babelfish): SQL
/// Server's types and rules, but a vector goes as its numbers in JSON text
/// (with a warning) and a vector index is left out with the reason, also
/// from SQL Server (the same family).
pub struct MsSqlNoVectors;

impl Dialect for MsSqlNoVectors {
    fn id(&self) -> &'static str {
        MsSql.id()
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        MsSql.parse_type(t)
    }

    fn render_type(&self, t: &L) -> Rendered {
        match t {
            L::Vector { .. } => super::vector_fallback(self, t),
            other => MsSql.render_type(other),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        MsSql.render_default(d, ty)
    }

    fn caps(&self) -> Caps {
        Caps {
            vector_indexes: false,
            vectors: false,
            ..MsSql.caps()
        }
    }

    fn implies_auto_increment(&self, t: &TypeSpec) -> bool {
        MsSql.implies_auto_increment(t)
    }

    fn target_refusal(&self, driver_id: &str) -> Option<&'static str> {
        MsSql.target_refusal(driver_id)
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        MsSql.finalize(t, report)
    }

    fn vector_index(
        &self,
        _: &dbine_driver::VectorIndex,
        _column: Option<&crate::logical::LogicalType>,
    ) -> Result<(Option<String>, dbine_driver::VectorIndex), String> {
        Err("Fabric Warehouse y Babelfish no tienen tipo vectorial ni índices vectoriales".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, ForeignKeyDef, IndexDef, KeyDef};

    fn ty(s: &str) -> L {
        MsSql.parse_type(&parse(s))
    }

    #[test]
    fn parses_driver_spellings() {
        assert_eq!(
            ty("nvarchar(50)"),
            L::Varchar {
                len: Some(50),
                unicode: true
            }
        );
        assert_eq!(ty("nvarchar(max)"), L::Text { unicode: true });
        assert_eq!(ty("varchar(max)"), L::Text { unicode: false });
        assert_eq!(ty("varbinary(max)"), L::Blob);
        assert_eq!(ty("varbinary(16)"), L::Varbinary { len: Some(16) });
        assert_eq!(
            ty("decimal(18,2)"),
            L::Decimal {
                precision: Some(18),
                scale: Some(2)
            }
        );
        assert_eq!(
            ty("datetime2(7)"),
            L::Timestamp {
                precision: Some(7),
                tz: false
            }
        );
        assert_eq!(
            ty("datetime2(0)"),
            L::Timestamp {
                precision: Some(0),
                tz: false
            }
        );
        assert_eq!(
            ty("datetimeoffset(3)"),
            L::Timestamp {
                precision: Some(3),
                tz: true
            }
        );
        assert_eq!(
            ty("time(7)"),
            L::Time {
                precision: Some(7),
                tz: false
            }
        );
        assert_eq!(
            ty("datetime"),
            L::Timestamp {
                precision: Some(3),
                tz: false
            }
        );
        assert_eq!(ty("float"), L::Float { bytes: 8 });
        assert_eq!(ty("real"), L::Float { bytes: 4 });
        assert_eq!(
            ty("tinyint"),
            L::Int {
                bytes: 1,
                unsigned: true
            }
        );
        assert_eq!(ty("timestamp"), L::RowVersion);
        assert_eq!(
            ty("sysname"),
            L::Varchar {
                len: Some(128),
                unicode: true
            }
        );
        assert!(matches!(ty("AS ([a]+(1)) PERSISTED"), L::Other { .. }));
    }

    #[test]
    fn sets_fit_every_member() {
        let r = MsSql.render_type(&L::Set {
            values: vec!["a".into(), "bb".into()],
        });
        assert_eq!(r.native, "nvarchar(4)");
    }

    #[test]
    fn finalize_sizes_keys_and_self_references() {
        let col = |n: &str, t: &str| ColumnDef {
            name: n.into(),
            data_type: t.into(),
            ..Default::default()
        };
        let mut t = TableSchema {
            name: "t".into(),
            columns: vec![
                ColumnDef {
                    auto_increment: true,
                    ..col("id", "decimal(38, 10)")
                },
                col("k", "nvarchar(max)"),
                col("p", "int"),
            ],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into()],
            }),
            indexes: vec![IndexDef {
                name: "ix".into(),
                columns: vec!["k".into()],
                ..Default::default()
            }],
            foreign_keys: vec![ForeignKeyDef {
                columns: vec!["p".into()],
                ref_table: "t".into(),
                ref_columns: vec!["id".into()],
                on_delete: Some("CASCADE".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut r = Report::default();
        MsSql.finalize(&mut t, &mut r);
        assert_eq!(t.columns[0].data_type, "bigint");
        assert_eq!(t.columns[1].data_type, "nvarchar(850)");
        assert_eq!(t.foreign_keys[0].on_delete, None);
    }

    /// Moved here from another family, a vector index keeps its column a
    /// `vector(n)`: a searched float array whose index states the
    /// dimension becomes one, and an index on what goes as JSON text (a
    /// sparse vector, a list without a dimension) is left out with why.
    #[test]
    fn vector_indexes_moved_into_sql_server() {
        use super::super::clickhouse::tests::vector_table;
        use crate::convert::{convert, Options};
        use crate::issue::IssueCode;
        let one = |t, from: &str| {
            let r = convert(&[t], from, "sqlserver", &Options::default()).unwrap();
            (r.tables[0].clone(), r.issues)
        };
        let (t, _) = one(vector_table("vector(3)", None, &[]), "postgres");
        assert_eq!(t.columns[1].data_type, "vector(3)");
        assert_eq!(t.indexes[0].kind.as_deref(), Some("VECTOR"));
        assert_eq!(
            t.indexes[0].vector.as_ref().unwrap().algorithm.as_deref(),
            Some("diskann")
        );
        let (t, _) = one(
            vector_table(
                "Array(Float32)",
                Some("vector_similarity"),
                &[("dimensions", "3")],
            ),
            "clickhouse",
        );
        assert_eq!(t.columns[1].data_type, "vector(3)");
        assert_eq!(t.indexes.len(), 1);
        for (t, from, why) in [
            (
                vector_table("sparsevec(5)", None, &[]),
                "postgres",
                "no es un vector en SQL Server",
            ),
            (
                vector_table("Array(Float32)", Some("vector_similarity"), &[]),
                "clickhouse",
                "su columna no es un vector",
            ),
        ] {
            let (t, issues) = one(t, from);
            assert!(t.indexes.is_empty(), "{from}");
            assert!(
                issues
                    .iter()
                    .any(|i| i.code == IssueCode::IndexDropped && i.message.contains(why)),
                "{issues:?}"
            );
        }
    }

    #[test]
    fn engines_without_vectors_take_them_as_json() {
        let v = L::Vector {
            dims: Some(3),
            element: dbine_driver::VectorElement::F32,
            sparse: false,
        };
        let r = MsSqlNoVectors.render_type(&v);
        assert!(!r.native.contains("vector"), "{}", r.native);
        assert_eq!(
            r.native,
            MsSql
                .render_type(&L::Array {
                    of: Box::new(L::Float { bytes: 4 })
                })
                .native
        );
        assert!(!r.notes.is_empty());
        // The rest is SQL Server's.
        assert_eq!(MsSqlNoVectors.render_type(&L::Bool).native, "bit");
        assert_eq!(MsSqlNoVectors.id(), "mssql");
        let e = MsSqlNoVectors
            .vector_index(&dbine_driver::VectorIndex::default(), None)
            .unwrap_err();
        assert!(e.contains("Fabric"), "{e}");
    }

    #[test]
    fn vectors_round_trip_and_cross_families() {
        use dbine_driver::{VectorElement as E, VectorIndex};
        let v = |dims, element, sparse| L::Vector {
            dims,
            element,
            sparse,
        };
        assert_eq!(ty("vector(3)"), v(Some(3), E::F32, false));
        assert_eq!(ty("vector(1536, float16)"), v(Some(1536), E::F16, false));
        assert_eq!(MsSql.render_type(&ty("vector(3)")).native, "vector(3)");
        assert_eq!(
            MsSql.render_type(&ty("vector(8, float16)")).native,
            "vector(8, float16)"
        );
        // pgvector's vector(3) and halfvec(3) as their logical types.
        assert_eq!(
            MsSql.render_type(&v(Some(3), E::F32, false)).native,
            "vector(3)"
        );
        // What SQL Server can't declare goes as JSON text, said.
        for odd in [
            v(None, E::F32, false),
            v(Some(5), E::F32, true),
            v(Some(5000), E::F32, false),
        ] {
            let r = MsSql.render_type(&odd);
            assert_eq!(r.native, "nvarchar(max)");
            assert!(!r.notes.is_empty());
        }
        let r = MsSql.render_type(&v(Some(4), E::F64, false));
        assert_eq!(r.native, "vector(4)");
        assert!(r.notes.iter().any(|n| n.severity == Severity::Loss));
        // A pgvector HNSW index: DiskANN with the same distance; one SQL
        // Server lacks is refused.
        let hnsw = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("l2".into()),
            params: [("m".to_string(), "16".to_string())].into(),
        };
        let (kind, mapped) = MsSql.vector_index(&hnsw, None).unwrap();
        assert_eq!(kind.as_deref(), Some("VECTOR"));
        assert_eq!(mapped.algorithm.as_deref(), Some("diskann"));
        assert_eq!(mapped.metric.as_deref(), Some("l2"));
        assert!(mapped.params.is_empty());
        let l1 = VectorIndex {
            metric: Some("l1".into()),
            ..Default::default()
        };
        assert!(MsSql.vector_index(&l1, None).is_err());
        // The column decides: float32 and float16 vectors are indexed, a
        // vector that goes as JSON text isn't.
        let col = v;
        for ok in [
            col(Some(3), E::F32, false),
            col(Some(3), E::F16, false),
            col(Some(3), E::F64, false),
            col(Some(3), E::Bit, false),
        ] {
            assert!(MsSql.vector_index(&hnsw, Some(&ok)).is_ok(), "{ok:?}");
        }
        for no in [
            col(Some(3), E::F32, true),
            col(None, E::F32, false),
            col(Some(4000), E::F32, false),
        ] {
            let e = MsSql.vector_index(&hnsw, Some(&no)).unwrap_err();
            assert!(e.contains("no es un vector"), "{no:?}: {e}");
        }
    }

    #[test]
    fn vector_tables_move_to_oracle_and_back() {
        use dbine_driver::{ColumnDef, IndexDef, VectorIndex};
        let t = TableSchema {
            name: "docs".into(),
            schema: Some("dbo".into()),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnDef {
                    name: "emb".into(),
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
                name: "ix_emb".into(),
                columns: vec!["emb".into()],
                kind: Some("VECTOR".into()),
                vector: Some(VectorIndex {
                    algorithm: Some("diskann".into()),
                    metric: Some("cosine".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = crate::convert::Options::default();
        let ora = crate::convert(&[t], "sqlserver", "oracle", &opts).unwrap();
        let emb = &ora.tables[0].columns[1];
        assert_eq!(emb.data_type, "VECTOR(3, FLOAT32)");
        let ix = &ora.tables[0].indexes[0];
        assert_eq!(ix.kind.as_deref(), Some("VECTOR"));
        let v = ix.vector.as_ref().unwrap();
        assert_eq!(v.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(v.metric.as_deref(), Some("cosine"));
        let back = crate::convert(&ora.tables, "oracle", "sqlserver", &opts).unwrap();
        assert_eq!(back.tables[0].columns[1].data_type, "vector(3)");
        let v = back.tables[0].indexes[0].vector.as_ref().unwrap();
        assert_eq!(v.algorithm.as_deref(), Some("diskann"));
        assert_eq!(v.metric.as_deref(), Some("cosine"));
    }
}
