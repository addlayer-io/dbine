//! SQL Server and the engines that speak T-SQL types (Azure SQL, Fabric
//! Warehouse, Babelfish).
//!
//! Types arrive as the driver builds them from `sys.columns`:
//! `TYPE_NAME(user_type_id)` plus the length in characters
//! (`nvarchar(50)`, `varchar(max)` for `max_length = -1`), `decimal(18,2)`
//! and the fractional digits of `datetime2(7)`, `time(7)`,
//! `datetimeoffset(7)`; `float` / `real` / `datetime` bare. Computed
//! columns arrive as `AS expr [PERSISTED]`.

use super::postgres::{fit_decimal, longest, precision_loss, prec, set_len, single_auto_increment, uuid_slot, UuidSlot};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{parse, TypeSpec};
use dbine_driver::TableSchema;

pub struct MsSql;

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
            "tinyint" => L::Int { bytes: 1, unsigned: true },
            "smallint" => L::int(2),
            "int" | "integer" => L::int(4),
            "bigint" => L::int(8),
            "decimal" | "numeric" | "dec" => L::Decimal { precision: p(0).or(Some(18)), scale: p(1).or(Some(0)) },
            "money" | "smallmoney" => L::Money,
            "real" => L::Float { bytes: 4 },
            "float" | "double precision" => L::Float { bytes: if p(0).is_some_and(|b| b <= 24) { 4 } else { 8 } },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: false },
            "nchar" | "national char" | "national character" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "character varying" | "char varying" if t.is_max() => L::Text { unicode: false },
            "varchar" | "character varying" | "char varying" => L::Varchar { len: p(0).or(Some(1)), unicode: false },
            "nvarchar" | "national character varying" | "national char varying" if t.is_max() => L::Text { unicode: true },
            "nvarchar" | "national character varying" | "national char varying" => L::Varchar { len: p(0).or(Some(1)), unicode: true },
            // Object names: nvarchar(128).
            "sysname" => L::Varchar { len: Some(128), unicode: true },
            "text" => L::Text { unicode: false },
            "ntext" => L::Text { unicode: true },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" | "binary varying" if t.is_max() => L::Blob,
            "varbinary" | "binary varying" => L::Varbinary { len: len().or(Some(1)) },
            "image" => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: frac(7), tz: false },
            "datetime" => L::Timestamp { precision: Some(3), tz: false },
            "smalldatetime" => L::Timestamp { precision: Some(0), tz: false },
            "datetime2" => L::Timestamp { precision: frac(7), tz: false },
            "datetimeoffset" => L::Timestamp { precision: frac(7), tz: true },
            "uniqueidentifier" => L::Uuid,
            "xml" => L::Xml,
            // SQL Server 2025 / Azure SQL.
            "json" => L::Json { binary: true },
            // In SQL Server `timestamp` is the old name of rowversion.
            "rowversion" | "timestamp" => L::RowVersion,
            "geometry" => L::Geometry { kind: None, srid: None, geography: false },
            "geography" => L::Geometry { kind: None, srid: None, geography: true },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
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
                L::Timestamp { precision: Some(p), .. } if *p <= 3 => "GETDATE()".into(),
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
            DefaultValue::Text(s) if matches!(ty, L::Char { unicode: true, .. } | L::Varchar { unicode: true, .. } | L::Text { unicode: true }) => {
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
        }
    }

    fn implies_auto_increment(&self, t: &TypeSpec) -> bool {
        t.has("identity")
    }

    /// What CREATE TABLE refuses otherwise: one IDENTITY of an integer
    /// type, no `(max)` column in a key, no cascading self-reference.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        single_auto_increment(t, report, "SQL Server");
        let table = t.name.clone();
        for c in t.columns.iter_mut().filter(|c| c.auto_increment) {
            let ok = matches!(
                self.parse_type(&parse(&c.data_type)),
                L::Int { .. } | L::Decimal { scale: Some(0) | None, .. }
            );
            if !ok {
                report.push(
                    Severity::Loss,
                    IssueCode::RangeLoss,
                    &table,
                    Some(&c.name),
                    format!("IDENTITY de SQL Server necesita un tipo entero: «{}» pasa a bigint.", c.data_type),
                );
                c.data_type = "bigint".into();
            }
        }

        // Key columns can't be (max), text, xml…: sized types within the key limit.
        let mut keys: Vec<(Vec<String>, u32, String)> = Vec::new();
        if let Some(pk) = &t.primary_key {
            keys.push((pk.columns.clone(), MAX_CLUSTERED_KEY, "la clave primaria".into()));
        }
        for ix in &t.indexes {
            keys.push((ix.columns.clone(), MAX_KEY, format!("el índice «{}»", ix.name)));
        }
        for (cols, limit, what) in keys {
            let budget = limit / cols.len().max(1) as u32;
            for name in &cols {
                let Some(c) = t.columns.iter_mut().find(|c| &c.name == name) else { continue };
                let to = match self.parse_type(&parse(&c.data_type)) {
                    L::Text { unicode: true } | L::Xml | L::Json { .. } => format!("nvarchar({})", budget / 2),
                    L::Text { unicode: false } => format!("varchar({budget})"),
                    L::Blob => format!("varbinary({budget})"),
                    _ => continue,
                };
                report.push(
                    Severity::Loss,
                    IssueCode::LengthLoss,
                    &table,
                    Some(name),
                    format!("SQL Server no admite «{}» en {what} (hasta {limit} bytes): pasa a {to}.", c.data_type),
                );
                c.data_type = to;
            }
        }

        // A self-reference can't cascade (it would form a cycle).
        for fk in t.foreign_keys.iter_mut().filter(|fk| fk.ref_table == table) {
            for (action, clause) in [(&mut fk.on_delete, "ON DELETE"), (&mut fk.on_update, "ON UPDATE")] {
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

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, ForeignKeyDef, IndexDef, KeyDef};

    fn ty(s: &str) -> L {
        MsSql.parse_type(&parse(s))
    }

    #[test]
    fn parses_driver_spellings() {
        assert_eq!(ty("nvarchar(50)"), L::Varchar { len: Some(50), unicode: true });
        assert_eq!(ty("nvarchar(max)"), L::Text { unicode: true });
        assert_eq!(ty("varchar(max)"), L::Text { unicode: false });
        assert_eq!(ty("varbinary(max)"), L::Blob);
        assert_eq!(ty("varbinary(16)"), L::Varbinary { len: Some(16) });
        assert_eq!(ty("decimal(18,2)"), L::Decimal { precision: Some(18), scale: Some(2) });
        assert_eq!(ty("datetime2(7)"), L::Timestamp { precision: Some(7), tz: false });
        assert_eq!(ty("datetime2(0)"), L::Timestamp { precision: Some(0), tz: false });
        assert_eq!(ty("datetimeoffset(3)"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(ty("time(7)"), L::Time { precision: Some(7), tz: false });
        assert_eq!(ty("datetime"), L::Timestamp { precision: Some(3), tz: false });
        assert_eq!(ty("float"), L::Float { bytes: 8 });
        assert_eq!(ty("real"), L::Float { bytes: 4 });
        assert_eq!(ty("tinyint"), L::Int { bytes: 1, unsigned: true });
        assert_eq!(ty("timestamp"), L::RowVersion);
        assert_eq!(ty("sysname"), L::Varchar { len: Some(128), unicode: true });
        assert!(matches!(ty("AS ([a]+(1)) PERSISTED"), L::Other { .. }));
    }

    #[test]
    fn sets_fit_every_member() {
        let r = MsSql.render_type(&L::Set { values: vec!["a".into(), "bb".into()] });
        assert_eq!(r.native, "nvarchar(4)");
    }

    #[test]
    fn finalize_sizes_keys_and_self_references() {
        let col = |n: &str, t: &str| ColumnDef { name: n.into(), data_type: t.into(), ..Default::default() };
        let mut t = TableSchema {
            name: "t".into(),
            columns: vec![ColumnDef { auto_increment: true, ..col("id", "decimal(38, 10)") }, col("k", "nvarchar(max)"), col("p", "int")],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            indexes: vec![IndexDef { name: "ix".into(), columns: vec!["k".into()], ..Default::default() }],
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
}
