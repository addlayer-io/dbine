//! BigQuery (GoogleSQL). Few types, all wide: INT64, NUMERIC / BIGNUMERIC,
//! FLOAT64, STRING, BYTES, DATETIME (no zone) vs TIMESTAMP (an instant),
//! ARRAY and STRUCT. No auto-increment and no indexes; primary and foreign
//! keys are informational (`NOT ENFORCED`) and take no actions.

use super::postgres::precision_loss;
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::convert::logical_of;
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{self, TypeSpec};
use dbine_driver::TableSchema;

pub struct BigQuery;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: BigQuery = BigQuery;
    (driver_id == "bigquery").then_some(&D as &dyn Dialect)
}

/// `ARRAY<INT64>`, `map<string,int>`, `STRUCT<a INT64, b STRING>` →
/// (`array`, the top-level arguments). `None` when there are no angle
/// brackets.
pub(super) fn generic(raw: &str) -> Option<(String, Vec<String>)> {
    let s = raw.trim();
    let open = s.find('<')?;
    if !s.ends_with('>') {
        return None;
    }
    let head = s[..open].trim().to_ascii_lowercase();
    Some((head, split_top(&s[open + 1..s.len() - 1])))
}

/// Split on commas outside `<>`, `()` and quotes.
pub(super) fn split_top(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth, mut quote) = (Vec::new(), String::new(), 0i32, false);
    for c in s.chars() {
        match c {
            '\'' | '"' | '`' => quote = !quote,
            '<' | '(' if !quote => depth += 1,
            '>' | ')' if !quote => depth -= 1,
            ',' if !quote && depth == 0 => {
                out.push(std::mem::take(&mut cur).trim().to_string());
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Logical type of a nested type spelling, with `d`'s names.
pub(super) fn nested(d: &dyn Dialect, s: &str) -> L {
    logical_of(d, &parse::parse(s))
}

/// A GoogleSQL string literal (`\'` escapes a quote there, `''` doesn't).
pub(super) fn gsql_literal(s: &str) -> String {
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

impl Dialect for BigQuery {
    fn id(&self) -> &'static str {
        "bigquery"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        if let Some((head, args)) = generic(&t.raw) {
            return match head.as_str() {
                "array" if args.len() == 1 => L::Array { of: Box::new(nested(self, &args[0])) },
                // A record: the closest neutral type is a document.
                "struct" => L::Json { binary: true },
                _ => L::Other { native: t.raw.clone() },
            };
        }
        match t.name.as_str() {
            "bool" | "boolean" => L::Bool,
            "int64" | "int" | "integer" | "bigint" | "smallint" | "tinyint" | "byteint" => L::int(8),
            "numeric" | "decimal" => match p(0) {
                Some(pr) => L::Decimal { precision: Some(pr), scale: p(1).or(Some(0)) },
                None => L::Decimal { precision: Some(38), scale: Some(9) },
            },
            "bignumeric" | "bigdecimal" => match p(0) {
                Some(pr) => L::Decimal { precision: Some(pr), scale: p(1).or(Some(0)) },
                None => L::Decimal { precision: Some(76), scale: Some(38) },
            },
            // The emulator reports FLOAT64 as DOUBLE.
            "float64" | "float" | "double" => L::Float { bytes: 8 },
            "string" => match p(0) {
                Some(n) => L::Varchar { len: Some(n), unicode: true },
                None => L::Text { unicode: true },
            },
            "bytes" => match p(0) {
                Some(n) => L::Varbinary { len: Some(n) },
                None => L::Blob,
            },
            "date" => L::Date,
            "time" => L::Time { precision: Some(6), tz: false },
            "datetime" => L::Timestamp { precision: Some(6), tz: false },
            "timestamp" => L::Timestamp { precision: Some(6), tz: true },
            "interval" => L::Interval,
            "json" => L::Json { binary: true },
            "geography" => L::Geometry { kind: None, srid: Some(4326), geography: true },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("BOOL"),
            L::Int { bytes, unsigned } => match L::signed_bytes_for(*bytes, *unsigned) {
                1..=8 => Rendered::exact("INT64"),
                _ if *bytes == 8 => Rendered::exact("NUMERIC(20, 0)").with(Info, TypeChanged, "Entero de 8 bytes sin signo como NUMERIC(20, 0)."),
                _ => Rendered::exact("BIGNUMERIC").with(Info, TypeChanged, "Entero de 16 bytes como BIGNUMERIC."),
            },
            L::Decimal { precision: Some(p), scale } => {
                let s = scale.unwrap_or(0);
                let int = p.saturating_sub(s);
                if s <= 9 && int <= 29 {
                    Rendered::exact(format!("NUMERIC({}, {s})", (*p).max(1)))
                } else if s <= 38 && int <= 38 {
                    Rendered::exact(format!("BIGNUMERIC({p}, {s})"))
                } else {
                    Rendered::exact("BIGNUMERIC").with(
                        Loss,
                        PrecisionLoss,
                        format!("BIGNUMERIC guarda hasta 38 dígitos enteros y 38 decimales; el origen es ({p}, {s})."),
                    )
                }
            }
            L::Decimal { precision: None, .. } => Rendered::exact("BIGNUMERIC")
                .with(Loss, PrecisionLoss, "El origen no fija la precisión: BIGNUMERIC guarda hasta 38 dígitos enteros y 38 decimales."),
            L::Float { .. } => Rendered::exact("FLOAT64"),
            L::Money => Rendered::exact("NUMERIC(19, 4)").with(Info, TypeChanged, "Moneda como NUMERIC(19, 4)."),
            L::Char { len, .. } => Rendered::exact(len.map_or("STRING".into(), |n| format!("STRING({n})")))
                .with(Info, TypeChanged, "BigQuery no tiene texto de largo fijo: no se rellena con espacios."),
            L::Varchar { len: Some(n), .. } => Rendered::exact(format!("STRING({n})")),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact("STRING"),
            L::Binary { len: Some(n) } | L::Varbinary { len: Some(n) } => Rendered::exact(format!("BYTES({n})")),
            L::Binary { len: None } | L::Varbinary { len: None } | L::Blob => Rendered::exact("BYTES"),
            L::Bit { .. } => Rendered::exact("BYTES").with(Warning, TypeApproximated, "BigQuery no tiene cadenas de bits: se guarda como binario."),
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => {
                let r = Rendered::exact("TIME").with_loss(precision_loss(*precision, 6));
                if *tz {
                    r.with(Loss, TimeZoneLoss, "BigQuery no guarda la zona horaria de una hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz: true } => Rendered::exact("TIMESTAMP").with_loss(precision_loss(*precision, 6)),
            L::Timestamp { precision, tz: false } => Rendered::exact("DATETIME").with_loss(precision_loss(*precision, 6)),
            L::Interval => Rendered::exact("INTERVAL"),
            L::Year => Rendered::exact("INT64").with(Info, TypeChanged, "Año como INT64."),
            L::Uuid => Rendered::exact("STRING").with(Info, TypeChanged, "UUID como STRING (BigQuery no tiene tipo UUID)."),
            L::Json { .. } => Rendered::exact("JSON"),
            L::Xml => Rendered::exact("STRING").with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } => Rendered::exact("STRING")
                .with(Warning, TypeApproximated, format!("BigQuery no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => Rendered::exact("ARRAY<STRING>")
                .with(Warning, TypeApproximated, format!("Conjunto como arreglo de texto. Valores: {}.", values.join(", "))),
            L::Array { of } if matches!(**of, L::Array { .. }) => {
                Rendered::exact("JSON").with(Warning, TypeApproximated, "BigQuery no tiene arreglos de arreglos: se guarda como JSON.")
            }
            L::Array { of } => {
                let inner = self.render_type(of);
                let r = Rendered { native: format!("ARRAY<{}>", inner.native), notes: inner.notes };
                r.with(Info, NullabilityChanged, "Los arreglos de BigQuery no admiten elementos NULL ni ser NULL (quedan vacíos).")
            }
            L::Map { .. } => Rendered::exact("JSON").with(Warning, TypeApproximated, "BigQuery no tiene mapas: se guarda como JSON."),
            L::Geometry { geography: true, .. } => Rendered::exact("GEOGRAPHY"),
            L::Geometry { .. } => Rendered::exact("GEOGRAPHY")
                .with(Warning, TypeApproximated, "BigQuery solo tiene GEOGRAPHY (WGS84, sobre la esfera): las geometrías planas cambian de sistema."),
            L::Inet => Rendered::exact("STRING").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("STRING").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("BYTES").with(Warning, TypeApproximated, "BigQuery no tiene versión de fila automática."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::Null => "NULL".into(),
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => gsql_literal(s),
            DefaultValue::Bool(b) if matches!(ty, L::Bool) => if *b { "TRUE" } else { "FALSE" }.into(),
            DefaultValue::Bool(b) => if *b { "1" } else { "0" }.into(),
            DefaultValue::CurrentTimestamp => match ty {
                L::Timestamp { tz: false, .. } => "CURRENT_DATETIME()".into(),
                L::Date => "CURRENT_DATE()".into(),
                _ => "CURRENT_TIMESTAMP()".into(),
            },
            DefaultValue::CurrentDate => "CURRENT_DATE()".into(),
            DefaultValue::CurrentTime => "CURRENT_TIME()".into(),
            DefaultValue::NewUuid => "GENERATE_UUID()".into(),
            DefaultValue::NextVal(_) | DefaultValue::Expr(_) => return None,
        })
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            on_delete: &[],
            on_update: &[],
            indexes: false,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 300,
            case: IdentCase::Preserve,
        }
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        not_enforced(t, report, "BigQuery");
    }
}

/// Keys the engine keeps as documentation only (BigQuery, Snowflake,
/// Databricks): reported once per table.
pub(super) fn not_enforced(t: &TableSchema, report: &mut Report, engine: &str) {
    if t.primary_key.as_ref().is_some_and(|k| !k.columns.is_empty()) {
        report.push(
            Severity::Info,
            IssueCode::OptionAdded,
            &t.name,
            Some("PRIMARY KEY"),
            format!("{engine} no hace cumplir la clave primaria (es informativa): no evita filas repetidas."),
        );
    }
    for fk in &t.foreign_keys {
        let label = fk.name.clone().unwrap_or_else(|| format!("→ {}", fk.ref_table));
        report.push(
            Severity::Info,
            IssueCode::OptionAdded,
            &t.name,
            Some(&label),
            format!("{engine} no hace cumplir las claves foráneas (son informativas)."),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn lt(s: &str) -> L {
        logical_of(&BigQuery, &parse(s))
    }

    #[test]
    fn generic_split() {
        assert_eq!(generic("ARRAY<STRUCT<a INT64, b STRING(10)>>"), Some(("array".into(), vec!["STRUCT<a INT64, b STRING(10)>".into()])));
        assert_eq!(generic("map<string,decimal(10,2)>"), Some(("map".into(), vec!["string".into(), "decimal(10,2)".into()])));
        assert_eq!(generic("INT64"), None);
    }

    #[test]
    fn parses_catalog_spellings() {
        assert_eq!(lt("INT64"), L::int(8));
        assert_eq!(lt("NUMERIC"), L::Decimal { precision: Some(38), scale: Some(9) });
        assert_eq!(lt("NUMERIC(10, 2)"), L::Decimal { precision: Some(10), scale: Some(2) });
        assert_eq!(lt("BIGNUMERIC"), L::Decimal { precision: Some(76), scale: Some(38) });
        assert_eq!(lt("FLOAT64"), L::Float { bytes: 8 });
        assert_eq!(lt("BOOL"), L::Bool);
        assert_eq!(lt("STRING(20)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(lt("STRING"), L::Text { unicode: true });
        assert_eq!(lt("BYTES(16)"), L::Varbinary { len: Some(16) });
        assert_eq!(lt("BYTES"), L::Blob);
        assert_eq!(lt("DATETIME"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(lt("TIMESTAMP"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(lt("TIME"), L::Time { precision: Some(6), tz: false });
        assert_eq!(lt("JSON"), L::Json { binary: true });
        assert_eq!(lt("INTERVAL"), L::Interval);
        assert_eq!(lt("ARRAY<STRING>"), L::Array { of: Box::new(L::Text { unicode: true }) });
        assert_eq!(lt("ARRAY<NUMERIC(5, 1)>"), L::Array { of: Box::new(L::Decimal { precision: Some(5), scale: Some(1) }) });
        assert_eq!(lt("STRUCT<city STRING, zip INT64>"), L::Json { binary: true });
        assert!(matches!(lt("GEOGRAPHY"), L::Geometry { geography: true, .. }));
        assert!(matches!(lt("RANGE<DATE>"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| BigQuery.render_type(&t);
        assert_eq!(r(L::Bool).native, "BOOL");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }).native, "INT64");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }).native, "NUMERIC(20, 0)");
        assert_eq!(r(L::int(16)).native, "BIGNUMERIC");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }).native, "NUMERIC(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(38), scale: Some(0) }).native, "BIGNUMERIC(38, 0)");
        assert_eq!(r(L::Decimal { precision: Some(20), scale: Some(12) }).native, "BIGNUMERIC(20, 12)");
        assert_eq!(r(L::Decimal { precision: Some(65), scale: Some(0) }).notes[0].code, IssueCode::PrecisionLoss);
        assert_eq!(r(L::Decimal { precision: None, scale: None }).native, "BIGNUMERIC");
        assert_eq!(r(L::Float { bytes: 4 }).native, "FLOAT64");
        assert_eq!(r(L::Money).native, "NUMERIC(19, 4)");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }).native, "STRING(3)");
        assert_eq!(r(L::Varchar { len: Some(30), unicode: false }).native, "STRING(30)");
        assert_eq!(r(L::Text { unicode: true }).native, "STRING");
        assert_eq!(r(L::Binary { len: Some(16) }).native, "BYTES(16)");
        assert_eq!(r(L::Varbinary { len: None }).native, "BYTES");
        assert_eq!(r(L::Blob).native, "BYTES");
        assert_eq!(r(L::Bit { len: Some(8) }).native, "BYTES");
        assert_eq!(r(L::Date).native, "DATE");
        assert_eq!(r(L::Time { precision: Some(7), tz: false }).notes[0].code, IssueCode::PrecisionLoss);
        assert_eq!(r(L::Timestamp { precision: Some(6), tz: true }).native, "TIMESTAMP");
        assert_eq!(r(L::Timestamp { precision: Some(3), tz: false }).native, "DATETIME");
        assert_eq!(r(L::Interval).native, "INTERVAL");
        assert_eq!(r(L::Year).native, "INT64");
        assert_eq!(r(L::Uuid).native, "STRING");
        assert_eq!(r(L::Json { binary: false }).native, "JSON");
        assert_eq!(r(L::Xml).native, "STRING");
        assert_eq!(r(L::Enum { values: vec!["a".into()] }).native, "STRING");
        assert_eq!(r(L::Set { values: vec!["a".into()] }).native, "ARRAY<STRING>");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }).native, "ARRAY<INT64>");
        assert_eq!(r(L::Array { of: Box::new(L::Array { of: Box::new(L::int(4)) }) }).native, "JSON");
        assert_eq!(r(L::Map { key: Box::new(L::int(4)), value: Box::new(L::int(4)) }).native, "JSON");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }).native, "GEOGRAPHY");
        assert_eq!(r(L::Inet).native, "STRING");
        assert_eq!(r(L::MacAddr).native, "STRING");
        assert_eq!(r(L::RowVersion).native, "BYTES");
    }

    #[test]
    fn defaults() {
        let d = BigQuery;
        assert_eq!(d.render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: false }).as_deref(), Some("CURRENT_DATETIME()"));
        assert_eq!(d.render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: true }).as_deref(), Some("CURRENT_TIMESTAMP()"));
        assert_eq!(d.render_default(&DefaultValue::NewUuid, &L::Uuid).as_deref(), Some("GENERATE_UUID()"));
        assert_eq!(d.render_default(&DefaultValue::Text("it's".into()), &L::Text { unicode: true }).as_deref(), Some("'it\\'s'"));
        assert_eq!(d.render_default(&DefaultValue::NextVal("s".into()), &L::int(8)), None);
    }
}
