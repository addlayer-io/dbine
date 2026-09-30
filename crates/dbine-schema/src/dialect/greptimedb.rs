//! GreptimeDB (MySQL protocol). Every table has a TIME INDEX: one
//! TIMESTAMP column, NOT NULL; the primary key are the tags that, with the
//! time index, identify a row (a new row with the same tags and time
//! replaces the old one). No foreign keys, indexes the designer offers,
//! TIME or INTERVAL columns.
//!
//! Also home of [`time_column`], shared by the time-series dialects that
//! need one timestamp column per table (TDengine, IoTDB).

use super::postgres::{longest, precision_loss};
use super::starrocks::{capped_decimal, unbounded_decimal};
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::{quote, DefaultValue};
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{self, TypeSpec};
use dbine_driver::{ColumnDef, TableSchema};

pub struct GreptimeDb;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: GreptimeDb = GreptimeDb;
    (driver_id == "greptimedb").then_some(&D as &dyn Dialect)
}

/// Column added when a table has no timestamp to index (the name
/// GreptimeDB itself gives it for tables created by ingestion).
pub const GREPTIME_TS: &str = "greptime_timestamp";

/// The column that should become the table's time axis: the first
/// timestamp column of the primary key, else the first timestamp column.
/// `is_ts` says whether a native type (in the target's spelling) is a
/// timestamp.
pub(crate) fn time_column(t: &TableSchema, is_ts: impl Fn(&TypeSpec) -> bool) -> Option<usize> {
    let pk: Vec<&String> = t.primary_key.iter().flat_map(|k| &k.columns).collect();
    let ts = |c: &ColumnDef| is_ts(&parse::parse(&c.data_type));
    t.columns
        .iter()
        .position(|c| pk.contains(&&c.name) && ts(c))
        .or_else(|| t.columns.iter().position(ts))
}

/// GreptimeDB's timestamp precisions: 0, 3, 6 or 9 digits.
fn ts_precision(p: Option<u8>) -> u8 {
    match p {
        Some(0) => 0,
        Some(1..=3) => 3,
        // Unknown precision: microseconds hold what most engines keep.
        None | Some(4..=6) => 6,
        _ => 9,
    }
}

impl Dialect for GreptimeDb {
    fn id(&self) -> &'static str {
        "greptimedb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let n = t.name.as_str();
        let u = t.unsigned;
        let ts = |digits: u8| L::Timestamp { precision: Some(digits), tz: true };
        match n {
            "boolean" | "bool" => L::Bool,
            "tinyint" | "int8" => L::Int { bytes: 1, unsigned: u },
            "smallint" | "int16" => L::Int { bytes: 2, unsigned: u },
            "int" | "integer" | "int32" => L::Int { bytes: 4, unsigned: u },
            "bigint" | "int64" => L::Int { bytes: 8, unsigned: u },
            "uint8" => L::Int { bytes: 1, unsigned: true },
            "uint16" => L::Int { bytes: 2, unsigned: true },
            "uint32" => L::Int { bytes: 4, unsigned: true },
            "uint64" => L::Int { bytes: 8, unsigned: true },
            "decimal" | "decimal128" | "numeric" => L::Decimal { precision: p(0).or(Some(38)), scale: p(1).or(Some(10)) },
            "float" | "float32" | "real" => L::Float { bytes: 4 },
            "double" | "float64" => L::Float { bytes: 8 },
            "string" | "varchar" | "text" | "char" => L::Text { unicode: true },
            "binary" | "varbinary" | "bytea" | "blob" => L::Blob,
            "date" => L::Date,
            // Timestamps are instants (epoch-based), shown in the session's zone.
            "timestamp" => ts(p(0).map_or(3, |x| x.min(9) as u8)),
            "datetime" => ts(6),
            "timestamp_s" | "timestamp_sec" | "timestamp_second" | "timestampsecond" => ts(0),
            "timestamp_ms" | "timestamp_millisecond" | "timestampmillisecond" => ts(3),
            "timestamp_us" | "timestamp_microsecond" | "timestampmicrosecond" => ts(6),
            "timestamp_ns" | "timestamp_nanosecond" | "timestampnanosecond" => ts(9),
            _ if n.starts_with("time") && !n.starts_with("timestamp") => L::Time { precision: None, tz: false },
            _ if n.starts_with("interval") => L::Interval,
            "json" => L::Json { binary: true },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let text = |why: String| Rendered::exact("STRING").with(Warning, TypeApproximated, why);
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => {
                let u = if *unsigned { " UNSIGNED" } else { "" };
                match bytes {
                    1 => Rendered::exact(format!("TINYINT{u}")),
                    2 => Rendered::exact(format!("SMALLINT{u}")),
                    3 | 4 => Rendered::exact(format!("INT{u}")),
                    8 => Rendered::exact(format!("BIGINT{u}")),
                    _ => Rendered::exact("DECIMAL(38, 0)").with(Loss, RangeLoss, "Entero de 16 bytes como DECIMAL(38, 0): los valores de 39 dígitos no entran."),
                }
            }
            L::Decimal { precision: Some(p), scale } => capped_decimal(*p, *scale, 38, "GreptimeDB"),
            L::Decimal { precision: None, .. } => unbounded_decimal("GreptimeDB"),
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(Info, TypeChanged, "Moneda como DECIMAL(19, 4)."),
            L::Char { .. } => Rendered::exact("STRING").with(Info, TypeChanged, "GreptimeDB no tiene texto de largo fijo: STRING sin límite ni relleno."),
            L::Varchar { .. } | L::Text { .. } => Rendered::exact("STRING"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("VARBINARY"),
            L::Bit { len } => match len {
                Some(n) if *n <= 64 => Rendered::exact("BIGINT UNSIGNED").with(Info, TypeChanged, "Cadena de bits como entero."),
                _ => Rendered::exact("VARBINARY").with(Warning, TypeApproximated, "Cadena de bits larga: se guarda como binario."),
            },
            L::Date => Rendered::exact("DATE"),
            L::Time { tz, .. } => {
                let r = text("GreptimeDB no admite columnas de hora: queda como texto HH:MM:SS.".into());
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Se pierde la zona horaria de la hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact(format!("TIMESTAMP({})", ts_precision(*precision))).with_loss(precision_loss(*precision, 9));
                if *tz {
                    r
                } else {
                    r.with(Info, TimeZoneLoss, "GreptimeDB guarda instantes: los valores sin zona se interpretan en la zona de la sesión (UTC por defecto).")
                }
            }
            L::Interval => text("GreptimeDB no admite columnas de intervalo: queda como texto.".into()),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            L::Uuid => Rendered::exact("STRING").with(Info, TypeChanged, "UUID como texto."),
            L::Json { .. } => Rendered::exact("JSON"),
            L::Xml => text("XML como texto.".into()),
            L::Enum { values } => text(format!("GreptimeDB no tiene enumerados: queda como texto. Valores: {} (hasta {} caracteres).", values.join(", "), longest(values))),
            L::Set { values } => text(format!("Conjunto como texto. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } => Rendered::exact("JSON").with(Warning, TypeApproximated, "GreptimeDB no tiene arreglos ni mapas en columnas: se guarda como JSON."),
            L::Geometry { .. } => text("Dato espacial como texto (WKT).".into()),
            L::Inet => Rendered::exact("STRING").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("STRING").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("VARBINARY").with(Warning, TypeApproximated, "GreptimeDB no tiene versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::Null => "NULL".into(),
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => quote(s),
            DefaultValue::Bool(b) if matches!(ty, L::Bool) => if *b { "true" } else { "false" }.into(),
            DefaultValue::Bool(b) => if *b { "1" } else { "0" }.into(),
            DefaultValue::CurrentTimestamp if matches!(ty, L::Timestamp { .. }) => "current_timestamp()".into(),
            _ => return None,
        })
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: false,
            on_delete: &[],
            on_update: &[],
            indexes: false,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 255,
            case: IdentCase::Preserve,
        }
    }

    /// The time index (`time_index`): the first timestamp of the key, else
    /// the first timestamp; a table without one gets `greptime_timestamp`
    /// filled with the insertion time.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        use IssueCode::*;
        use Severity::*;
        let table = t.name.clone();
        let ts_col = match t.options.get("time_index").filter(|c| !c.trim().is_empty()) {
            Some(c) => c.clone(),
            None => {
                let name = match time_column(t, |s| s.name.starts_with("timestamp")) {
                    Some(i) => {
                        let name = t.columns[i].name.clone();
                        report.push(Info, OptionAdded, &table, Some("time_index"), format!("«{name}» es el índice de tiempo (TIME INDEX) de la tabla."));
                        name
                    }
                    None => {
                        t.columns.push(ColumnDef {
                            name: GREPTIME_TS.into(),
                            data_type: "TIMESTAMP(3)".into(),
                            nullable: false,
                            default_value: Some("current_timestamp()".into()),
                            ..Default::default()
                        });
                        report.push(
                            Warning,
                            OptionAdded,
                            &table,
                            Some(GREPTIME_TS),
                            format!("GreptimeDB exige una columna de tiempo: se agrega «{GREPTIME_TS}» con la hora de inserción."),
                        );
                        GREPTIME_TS.to_string()
                    }
                };
                t.options.insert("time_index".into(), name.clone());
                name
            }
        };
        if let Some(c) = t.columns.iter_mut().find(|c| c.name == ts_col && c.nullable) {
            c.nullable = false;
            report.push(Info, NullabilityChanged, &table, Some(&ts_col), "El índice de tiempo no admite nulos: la columna queda NOT NULL.");
        }
        let tags: Vec<String> = t.primary_key.iter().flat_map(|k| &k.columns).filter(|c| **c != ts_col).cloned().collect();
        if tags.is_empty() {
            report.push(
                Warning,
                PrimaryKeyDropped,
                &table,
                Some(&ts_col),
                format!("Sin clave primaria, dos filas con el mismo «{ts_col}» se reemplazan: la tabla guarda una fila por instante."),
            );
        } else {
            report.push(
                Info,
                PrimaryKeyAdded,
                &table,
                Some(&tags.join(", ")),
                format!("La clave primaria pasa a ser las etiquetas ({}) más «{ts_col}»: una fila se identifica por las dos.", tags.join(", ")),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn p(s: &str) -> L {
        crate::convert::logical_of(&GreptimeDb, &parse(s))
    }

    #[test]
    fn parses_catalog_spellings() {
        assert_eq!(p("timestamp(3)"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(p("TimestampNanosecond"), L::Timestamp { precision: Some(9), tz: true });
        assert_eq!(p("string"), L::Text { unicode: true });
        assert_eq!(p("boolean"), L::Bool);
        assert_eq!(p("tinyint"), L::int(1));
        assert_eq!(p("int unsigned"), L::Int { bytes: 4, unsigned: true });
        assert_eq!(p("UInt64"), L::Int { bytes: 8, unsigned: true });
        assert_eq!(p("bigint"), L::int(8));
        assert_eq!(p("decimal(10,2)"), L::Decimal { precision: Some(10), scale: Some(2) });
        assert_eq!(p("float"), L::Float { bytes: 4 });
        assert_eq!(p("Float64"), L::Float { bytes: 8 });
        assert_eq!(p("date"), L::Date);
        assert_eq!(p("json"), L::Json { binary: true });
        assert_eq!(p("varbinary"), L::Blob);
        assert_eq!(p("TimeMillisecond"), L::Time { precision: None, tz: false });
        assert!(matches!(p("vector(3)"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| GreptimeDb.render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::Int { bytes: 2, unsigned: true }), "SMALLINT UNSIGNED");
        assert_eq!(r(L::int(3)), "INT");
        assert_eq!(r(L::int(16)), "DECIMAL(38, 0)");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "DECIMAL(12, 2)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "DECIMAL(38, 10)");
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Float { bytes: 8 }), "DOUBLE");
        assert_eq!(r(L::Money), "DECIMAL(19, 4)");
        assert_eq!(r(L::Char { len: Some(2), unicode: true }), "STRING");
        assert_eq!(r(L::Varchar { len: Some(20), unicode: true }), "STRING");
        assert_eq!(r(L::Text { unicode: true }), "STRING");
        assert_eq!(r(L::Binary { len: None }), "VARBINARY");
        assert_eq!(r(L::Blob), "VARBINARY");
        assert_eq!(r(L::Bit { len: Some(1) }), "BIGINT UNSIGNED");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: None, tz: false }), "STRING");
        assert_eq!(r(L::Timestamp { precision: None, tz: true }), "TIMESTAMP(6)");
        assert_eq!(r(L::Timestamp { precision: Some(0), tz: true }), "TIMESTAMP(0)");
        assert_eq!(r(L::Timestamp { precision: Some(2), tz: true }), "TIMESTAMP(3)");
        assert_eq!(r(L::Timestamp { precision: Some(7), tz: true }), "TIMESTAMP(9)");
        assert_eq!(r(L::Interval), "STRING");
        assert_eq!(r(L::Year), "SMALLINT");
        assert_eq!(r(L::Uuid), "STRING");
        assert_eq!(r(L::Json { binary: false }), "JSON");
        assert_eq!(r(L::Xml), "STRING");
        assert_eq!(r(L::Enum { values: vec!["a".into()] }), "STRING");
        assert_eq!(r(L::Set { values: vec!["a".into()] }), "STRING");
        assert_eq!(r(L::Array { of: Box::new(L::Bool) }), "JSON");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "JSON");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: true }), "STRING");
        assert_eq!(r(L::Inet), "STRING");
        assert_eq!(r(L::MacAddr), "STRING");
        assert_eq!(r(L::RowVersion), "VARBINARY");
    }

    #[test]
    fn finalize_picks_or_adds_the_time_index() {
        let col = |n: &str, ty: &str| ColumnDef { name: n.into(), data_type: ty.into(), nullable: true, ..Default::default() };
        let mut t = TableSchema { name: "t".into(), columns: vec![col("id", "BIGINT"), col("alta", "TIMESTAMP(6)")], ..Default::default() };
        t.primary_key = Some(dbine_driver::KeyDef { name: None, columns: vec!["id".into()] });
        let mut rep = Report::default();
        GreptimeDb.finalize(&mut t, &mut rep);
        assert_eq!(t.options.get("time_index").map(String::as_str), Some("alta"));
        assert!(!t.columns[1].nullable);

        let mut t = TableSchema { name: "t".into(), columns: vec![col("id", "BIGINT")], ..Default::default() };
        let mut rep = Report::default();
        GreptimeDb.finalize(&mut t, &mut rep);
        assert_eq!(t.options.get("time_index").map(String::as_str), Some(GREPTIME_TS));
        assert_eq!(t.columns.last().unwrap().default_value.as_deref(), Some("current_timestamp()"));
        assert!(rep.issues.iter().any(|i| i.code == IssueCode::OptionAdded && i.severity == Severity::Warning));
    }
}
