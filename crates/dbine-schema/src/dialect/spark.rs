//! The Hive type system and its engines: Hive (and Cloudera's), Impala,
//! Spark Thrift Server / Kyuubi, Databricks (Delta) and Athena's DDL
//! (Hive-style, Iceberg tables by default).
//!
//! Differences that matter:
//! - Hive, Spark, Impala and Athena tables have no keys, NOT NULL or
//!   defaults (Impala only on Kudu tables); Databricks has NOT NULL,
//!   defaults, identity columns (BIGINT) and informational keys.
//! - TIMESTAMP is a plain date-time in Hive and Impala (to the nanosecond)
//!   and an instant in Spark / Databricks (to the microsecond), which have
//!   TIMESTAMP_NTZ for the plain one.
//! - Impala's REAL is a DOUBLE (Spark's is a FLOAT); Impala only creates
//!   complex types in Parquet / ORC tables, so they go as JSON text.
//! - Athena's Iceberg tables have no TINYINT / SMALLINT / CHAR / VARCHAR.
//! - Names are case-insensitive and stored in lower case.

use super::bigquery::{generic, nested, not_enforced};
use super::postgres::precision_loss;
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flavor {
    Hive,
    Impala,
    Spark,
    Databricks,
    Athena,
}

pub struct Hive {
    flavor: Flavor,
}

static HIVE: Hive = Hive { flavor: Flavor::Hive };
static IMPALA: Hive = Hive { flavor: Flavor::Impala };
static SPARK: Hive = Hive { flavor: Flavor::Spark };
static DATABRICKS: Hive = Hive { flavor: Flavor::Databricks };
static ATHENA: Hive = Hive { flavor: Flavor::Athena };

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    Some(match driver_id {
        "hive" | "cloudera" => &HIVE,
        "impala" => &IMPALA,
        "spark" | "kyuubi" => &SPARK,
        "databricks" | "azure_databricks" => &DATABRICKS,
        _ => return None,
    })
}

/// Athena's DDL (served from `trino.rs`, whose family runs its queries).
pub(super) fn athena() -> &'static dyn Dialect {
    &ATHENA
}

/// A Spark / Hive string literal: quotes and backslashes escaped with a
/// backslash (`''` would be two literals).
fn literal(s: &str) -> String {
    let mut out = String::from("'");
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

impl Hive {
    fn f(&self) -> Flavor {
        self.flavor
    }

    /// Complex column types exist (Impala: only in Parquet / ORC tables).
    fn complex(&self) -> bool {
        self.f() != Flavor::Impala
    }

    fn engine(&self) -> &'static str {
        match self.f() {
            Flavor::Hive => "Hive",
            Flavor::Impala => "Impala",
            Flavor::Spark => "Spark",
            Flavor::Databricks => "Databricks",
            Flavor::Athena => "Athena",
        }
    }

    fn string_note(&self, r: Rendered, what: &str) -> Rendered {
        r.with(Severity::Warning, IssueCode::TypeApproximated, format!("{} no tiene {what}: queda como texto.", self.engine()))
    }
}

impl Dialect for Hive {
    fn id(&self) -> &'static str {
        match self.f() {
            Flavor::Hive => "hive",
            Flavor::Impala => "impala",
            Flavor::Spark => "spark",
            Flavor::Databricks => "databricks",
            Flavor::Athena => "athena",
        }
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        if let Some((head, args)) = generic(&t.raw) {
            return match head.as_str() {
                "array" if args.len() == 1 => L::Array { of: Box::new(nested(self, &args[0])) },
                "map" if args.len() == 2 => L::Map { key: Box::new(nested(self, &args[0])), value: Box::new(nested(self, &args[1])) },
                "struct" => L::Json { binary: true },
                _ => L::Other { native: t.raw.clone() },
            };
        }
        let spark_like = matches!(self.f(), Flavor::Spark | Flavor::Databricks);
        match t.name.as_str() {
            "boolean" | "bool" => L::Bool,
            "tinyint" | "byte" => L::int(1),
            "smallint" | "short" => L::int(2),
            "int" | "integer" => L::int(4),
            "bigint" | "long" => L::int(8),
            "real" if self.f() == Flavor::Impala => L::Float { bytes: 8 },
            "float" | "real" => L::Float { bytes: 4 },
            "double" | "double precision" => L::Float { bytes: 8 },
            "decimal" | "dec" | "numeric" => L::Decimal {
                precision: p(0).or(Some(if self.f() == Flavor::Impala { 9 } else { 10 })),
                scale: p(1).or(Some(0)),
            },
            "string" => L::Text { unicode: true },
            "varchar" => L::Varchar { len: p(0), unicode: true },
            "char" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "binary" => L::Blob,
            "date" => L::Date,
            "timestamp" if t.with_tz => L::Timestamp { precision: Some(9), tz: true },
            "timestamp" if spark_like => L::Timestamp { precision: Some(6), tz: true },
            "timestamp" => L::Timestamp { precision: Some(if self.f() == Flavor::Athena { 6 } else { 9 }), tz: false },
            "timestamp_ntz" => L::Timestamp { precision: Some(6), tz: false },
            "timestamp_ltz" => L::Timestamp { precision: Some(6), tz: true },
            "variant" => L::Json { binary: true },
            _ if t.name.starts_with("interval") => L::Interval,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let f = self.f();
        let iceberg = f == Flavor::Athena;
        let s = || Rendered::exact("STRING");
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => {
                let r = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 if !iceberg => Rendered::exact("TINYINT"),
                    2 if !iceberg => Rendered::exact("SMALLINT"),
                    1..=4 => Rendered::exact("INT"),
                    8 => Rendered::exact("BIGINT"),
                    _ if *bytes == 8 => Rendered::exact("DECIMAL(20, 0)").with(Info, TypeChanged, "Entero de 8 bytes sin signo como DECIMAL(20, 0)."),
                    _ => Rendered::exact("DECIMAL(38, 0)").with(Loss, RangeLoss, "Entero de 16 bytes como DECIMAL(38, 0): no entran los valores de 39 dígitos."),
                };
                if iceberg && *bytes < 4 {
                    r.with(Info, TypeChanged, "Las tablas Iceberg no tienen TINYINT ni SMALLINT: queda INT.")
                } else if *unsigned && *bytes < 8 {
                    r.with(Info, TypeChanged, format!("{} no tiene enteros sin signo: se usa un tipo más grande con signo.", self.engine()))
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } if *p <= 38 => Rendered::exact(format!("DECIMAL({p}, {})", scale.unwrap_or(0))),
            L::Decimal { precision: Some(p), scale } => Rendered::exact(format!("DECIMAL(38, {})", scale.unwrap_or(0).min(38)))
                .with(Loss, PrecisionLoss, format!("{} admite hasta 38 dígitos; el origen tiene {p}.", self.engine())),
            L::Decimal { precision: None, .. } => Rendered::exact("DECIMAL(38, 10)")
                .with(Loss, PrecisionLoss, "El origen no fija la precisión: se usa DECIMAL(38, 10)."),
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(Info, TypeChanged, "Moneda como DECIMAL(19, 4)."),
            L::Char { .. } | L::Varchar { len: Some(_), .. } if iceberg => {
                s().with(Info, LengthLoss, "Las tablas Iceberg no tienen CHAR ni VARCHAR: queda STRING, sin largo.")
            }
            L::Char { len, .. } => match len.unwrap_or(1) {
                n if n <= 255 => Rendered::exact(format!("CHAR({n})")),
                n if n <= 65_535 || f == Flavor::Databricks => {
                    Rendered::exact(format!("VARCHAR({n})")).with(Info, TypeChanged, "CHAR admite hasta 255: se usa VARCHAR.")
                }
                _ => s().with(Info, TypeChanged, "Texto fijo largo como STRING."),
            },
            L::Varchar { len: Some(n), .. } if *n <= 65_535 || f == Flavor::Databricks => Rendered::exact(format!("VARCHAR({n})")),
            L::Varchar { .. } | L::Text { .. } => s(),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => {
                let r = Rendered::exact("BINARY");
                let r = if matches!(t, L::Binary { len: Some(_) } | L::Varbinary { len: Some(_) }) {
                    r.with(Info, LengthLoss, "BINARY no limita el largo.")
                } else {
                    r
                };
                if f == Flavor::Impala {
                    r.with(Info, TypeChanged, "BINARY existe desde Impala 4.1.")
                } else {
                    r
                }
            }
            L::Bit { .. } => Rendered::exact("BINARY").with(Warning, TypeApproximated, "No hay cadenas de bits: se guarda como binario."),
            L::Date => Rendered::exact("DATE"),
            L::Time { tz, .. } => {
                let r = self.string_note(s(), "tipo hora");
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Se pierde la zona horaria de la hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => match f {
                Flavor::Spark | Flavor::Databricks => {
                    Rendered::exact(if *tz { "TIMESTAMP" } else { "TIMESTAMP_NTZ" }).with_loss(precision_loss(*precision, 6))
                }
                _ => {
                    let max = if iceberg { 6 } else { 9 };
                    let r = Rendered::exact("TIMESTAMP").with_loss(precision_loss(*precision, max));
                    if *tz {
                        r.with(Loss, TimeZoneLoss, format!("TIMESTAMP de {} no guarda zona horaria: conviene guardar en UTC.", self.engine()))
                    } else {
                        r
                    }
                }
            },
            L::Interval => self.string_note(s(), "intervalos en columnas"),
            L::Year => Rendered::exact(if iceberg { "INT" } else { "SMALLINT" }).with(Info, TypeChanged, "Año como entero."),
            L::Uuid => s().with(Info, TypeChanged, "UUID como STRING."),
            L::Json { .. } => s().with(Info, TypeChanged, "JSON como texto."),
            L::Xml => s().with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } => s().with(Warning, TypeApproximated, format!("No hay enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } if self.complex() => Rendered::exact("ARRAY<STRING>")
                .with(Warning, TypeApproximated, format!("Conjunto como arreglo de texto. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } | L::Set { .. } if !self.complex() => s().with(
                Warning,
                TypeApproximated,
                "Impala solo crea tipos complejos en tablas Parquet u ORC: queda como JSON en texto.",
            ),
            L::Array { of } => {
                let inner = self.render_type(of);
                Rendered { native: format!("ARRAY<{}>", inner.native), notes: inner.notes }
            }
            L::Map { key, value } => {
                let (k, v) = (self.render_type(key), self.render_type(value));
                let mut notes = k.notes;
                notes.extend(v.notes);
                Rendered { native: format!("MAP<{}, {}>", k.native, v.native), notes }
            }
            L::Set { .. } => unreachable!(),
            L::Geometry { .. } => s().with(Warning, TypeApproximated, "Dato espacial como texto (WKT)."),
            L::Inet => s().with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => s().with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("BINARY").with(Warning, TypeApproximated, "No hay versión de fila automática."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::Null => "NULL".into(),
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => literal(s),
            DefaultValue::Bool(b) if matches!(ty, L::Bool) => if *b { "TRUE" } else { "FALSE" }.into(),
            DefaultValue::Bool(b) => if *b { "1" } else { "0" }.into(),
            DefaultValue::CurrentTimestamp if matches!(ty, L::Date) => "current_date()".into(),
            DefaultValue::CurrentTimestamp => "current_timestamp()".into(),
            DefaultValue::CurrentDate => "current_date()".into(),
            DefaultValue::CurrentTime => "date_format(current_timestamp(), 'HH:mm:ss')".into(),
            DefaultValue::NewUuid => "uuid()".into(),
            DefaultValue::NextVal(_) | DefaultValue::Expr(_) => return None,
        })
    }

    fn caps(&self) -> Caps {
        let delta = self.f() == Flavor::Databricks;
        Caps {
            foreign_keys: delta,
            on_delete: &[],
            on_update: &[],
            indexes: false,
            partial_indexes: false,
            supports_include: false,
            auto_increment: delta,
            defaults: delta,
            nullability: delta,
            comments: true,
            max_identifier: if delta { 255 } else { 128 },
            case: IdentCase::Lower,
        }
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        use IssueCode::*;
        use Severity::*;
        if self.f() == Flavor::Databricks {
            for c in t.columns.iter_mut().filter(|c| c.auto_increment && c.data_type != "BIGINT") {
                let widen = matches!(c.data_type.as_str(), "TINYINT" | "SMALLINT" | "INT");
                let msg = format!("Las columnas de identidad de Delta son BIGINT: {} pasa a BIGINT.", c.data_type);
                report.push(if widen { Info } else { Loss }, if widen { TypeChanged } else { RangeLoss }, &t.name, Some(&c.name), msg);
                c.data_type = "BIGINT".into();
            }
            not_enforced(t, report, "Databricks");
            return;
        }
        if let Some(k) = t.primary_key.take().filter(|k| !k.columns.is_empty()) {
            let msg = if self.f() == Flavor::Impala {
                "Impala solo tiene clave primaria en tablas Kudu (formato KUDU): se omite.".to_string()
            } else {
                format!("{} no tiene claves primarias: se omite ({}).", self.engine(), k.columns.join(", "))
            };
            report.push(Dropped, PrimaryKeyDropped, &t.name, None, msg);
        }
        if self.f() == Flavor::Athena {
            if !t.options.contains_key("table_type") {
                t.options.insert("table_type".into(), "iceberg".into());
                report.push(Info, OptionAdded, &t.name, Some("table_type"), "Tabla Iceberg, la que Athena crea y modifica con SQL.");
            }
            if !t.options.contains_key("location") {
                report.push(Warning, OptionAdded, &t.name, Some("location"), "Athena necesita la ubicación S3 de la tabla (location): hay que completarla.");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::logical_of;
    use crate::parse::parse;
    use dbine_driver::{ColumnDef, KeyDef};

    fn lt(d: &dyn Dialect, s: &str) -> L {
        logical_of(d, &parse(s))
    }

    #[test]
    fn ids() {
        for id in ["hive", "cloudera", "impala", "spark", "kyuubi", "databricks", "azure_databricks"] {
            assert!(lookup(id).is_some(), "{id}");
        }
        assert_eq!(athena().id(), "athena");
    }

    #[test]
    fn parses_catalog_spellings() {
        let h = &HIVE;
        assert_eq!(lt(h, "int"), L::int(4));
        assert_eq!(lt(h, "TINYINT"), L::int(1));
        assert_eq!(lt(h, "decimal(10,2)"), L::Decimal { precision: Some(10), scale: Some(2) });
        assert_eq!(lt(h, "decimal"), L::Decimal { precision: Some(10), scale: Some(0) });
        assert_eq!(lt(&IMPALA, "DECIMAL"), L::Decimal { precision: Some(9), scale: Some(0) });
        assert_eq!(lt(h, "float"), L::Float { bytes: 4 });
        assert_eq!(lt(&IMPALA, "REAL"), L::Float { bytes: 8 });
        assert_eq!(lt(&SPARK, "real"), L::Float { bytes: 4 });
        assert_eq!(lt(h, "string"), L::Text { unicode: true });
        assert_eq!(lt(h, "varchar(20)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(lt(h, "char(2)"), L::Char { len: Some(2), unicode: true });
        assert_eq!(lt(h, "binary"), L::Blob);
        assert_eq!(lt(h, "timestamp"), L::Timestamp { precision: Some(9), tz: false });
        assert_eq!(lt(h, "timestamp with local time zone"), L::Timestamp { precision: Some(9), tz: true });
        assert_eq!(lt(&DATABRICKS, "timestamp"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(lt(&DATABRICKS, "timestamp_ntz"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(lt(&DATABRICKS, "variant"), L::Json { binary: true });
        assert_eq!(lt(h, "array<string>"), L::Array { of: Box::new(L::Text { unicode: true }) });
        assert_eq!(lt(h, "map<string,decimal(10,2)>"), L::Map {
            key: Box::new(L::Text { unicode: true }),
            value: Box::new(L::Decimal { precision: Some(10), scale: Some(2) })
        });
        assert_eq!(lt(h, "struct<a:int,b:string>"), L::Json { binary: true });
        assert_eq!(lt(&DATABRICKS, "INTERVAL DAY TO SECOND"), L::Interval);
        assert!(matches!(lt(h, "uniontype<int,string>"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| DATABRICKS.render_type(&t);
        assert_eq!(r(L::Bool).native, "BOOLEAN");
        assert_eq!(r(L::int(1)).native, "TINYINT");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }).native, "BIGINT");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }).native, "DECIMAL(20, 0)");
        assert_eq!(r(L::int(16)).notes[0].code, IssueCode::RangeLoss);
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }).native, "DECIMAL(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(65), scale: Some(30) }).native, "DECIMAL(38, 30)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }).native, "DECIMAL(38, 10)");
        assert_eq!(r(L::Float { bytes: 4 }).native, "FLOAT");
        assert_eq!(r(L::Float { bytes: 8 }).native, "DOUBLE");
        assert_eq!(r(L::Money).native, "DECIMAL(19, 4)");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }).native, "CHAR(3)");
        assert_eq!(r(L::Char { len: Some(300), unicode: true }).native, "VARCHAR(300)");
        assert_eq!(r(L::Varchar { len: Some(30), unicode: true }).native, "VARCHAR(30)");
        assert_eq!(r(L::Text { unicode: true }).native, "STRING");
        assert_eq!(r(L::Binary { len: Some(16) }).native, "BINARY");
        assert_eq!(r(L::Blob).native, "BINARY");
        assert_eq!(r(L::Bit { len: None }).native, "BINARY");
        assert_eq!(r(L::Date).native, "DATE");
        assert_eq!(r(L::Time { precision: None, tz: false }).native, "STRING");
        assert_eq!(r(L::Timestamp { precision: Some(6), tz: true }).native, "TIMESTAMP");
        assert_eq!(r(L::Timestamp { precision: Some(3), tz: false }).native, "TIMESTAMP_NTZ");
        assert_eq!(r(L::Interval).native, "STRING");
        assert_eq!(r(L::Year).native, "SMALLINT");
        assert_eq!(r(L::Uuid).native, "STRING");
        assert_eq!(r(L::Json { binary: true }).native, "STRING");
        assert_eq!(r(L::Xml).native, "STRING");
        assert_eq!(r(L::Enum { values: vec!["a".into()] }).native, "STRING");
        assert_eq!(r(L::Set { values: vec!["a".into()] }).native, "ARRAY<STRING>");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }).native, "ARRAY<INT>");
        assert_eq!(r(L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::int(8)) }).native, "MAP<STRING, BIGINT>");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }).native, "STRING");
        assert_eq!(r(L::Inet).native, "STRING");
        assert_eq!(r(L::MacAddr).native, "STRING");
        assert_eq!(r(L::RowVersion).native, "BINARY");
    }

    #[test]
    fn flavor_differences() {
        assert_eq!(HIVE.render_type(&L::Timestamp { precision: Some(6), tz: false }).native, "TIMESTAMP");
        assert!(HIVE.render_type(&L::Timestamp { precision: Some(6), tz: true }).notes.iter().any(|n| n.code == IssueCode::TimeZoneLoss));
        assert_eq!(IMPALA.render_type(&L::Array { of: Box::new(L::int(4)) }).native, "STRING");
        assert_eq!(HIVE.render_type(&L::Varchar { len: Some(70_000), unicode: true }).native, "STRING");
        assert_eq!(ATHENA.render_type(&L::int(2)).native, "INT");
        assert_eq!(ATHENA.render_type(&L::Varchar { len: Some(20), unicode: true }).native, "STRING");
        assert!(!HIVE.caps().nullability && DATABRICKS.caps().nullability);
        assert_eq!(DATABRICKS.render_default(&DefaultValue::Text("it's".into()), &L::Text { unicode: true }).as_deref(), Some("'it\\'s'"));
        assert_eq!(DATABRICKS.render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: true }).as_deref(), Some("current_timestamp()"));
    }

    #[test]
    fn finalize_per_flavor() {
        let t = || TableSchema {
            name: "t".into(),
            columns: vec![ColumnDef { name: "id".into(), data_type: "INT".into(), auto_increment: true, ..Default::default() }],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            ..Default::default()
        };
        let mut x = t();
        let mut rep = Report::default();
        HIVE.finalize(&mut x, &mut rep);
        assert!(x.primary_key.is_none());
        assert!(rep.issues.iter().any(|i| i.code == IssueCode::PrimaryKeyDropped));

        let mut x = t();
        let mut rep = Report::default();
        DATABRICKS.finalize(&mut x, &mut rep);
        assert_eq!(x.columns[0].data_type, "BIGINT");
        assert!(x.primary_key.is_some());

        let mut x = t();
        let mut rep = Report::default();
        ATHENA.finalize(&mut x, &mut rep);
        assert_eq!(x.options.get("table_type").map(String::as_str), Some("iceberg"));
        assert!(rep.issues.iter().any(|i| i.object.as_deref() == Some("location")));
    }
}
