//! PostgreSQL and the engines that speak its type system (TimescaleDB,
//! YugabyteDB, CockroachDB, Aurora/AlloyDB/Cloud SQL, Redshift…).
//!
//! Types arrive as `format_type(atttypid, atttypmod)` spells them:
//! `character varying(20)`, `numeric(10,2)`, `timestamp(3) with time zone`,
//! `time without time zone`, `bit varying(5)`, `interval day to second(3)`,
//! `integer[]`, `"char"`; generated columns carry
//! `… GENERATED ALWAYS AS (expr) STORED` after the type.

use super::{standard_default, Caps, Dialect, IdentCase, Rendered, ALL_ACTIONS};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct Postgres;

/// Longest `varchar(n)` PostgreSQL accepts.
const MAX_VARCHAR: u32 = 10_485_760;
/// Largest declared `numeric` precision.
const MAX_NUMERIC: u32 = 1000;

impl Dialect for Postgres {
    fn id(&self) -> &'static str {
        "postgres"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let frac = || p(0).map(|x| x.min(255) as u8);
        match t.name.as_str() {
            "bool" | "boolean" => L::Bool,
            "smallint" | "int2" | "smallserial" | "serial2" => L::int(2),
            "integer" | "int" | "int4" | "serial" | "serial4" => L::int(4),
            "bigint" | "int8" | "bigserial" | "serial8" => L::int(8),
            // Object identifiers: unsigned 4-byte.
            "oid" | "regclass" | "regtype" | "regproc" | "xid" => L::Int { bytes: 4, unsigned: true },
            "numeric" | "decimal" => L::Decimal { precision: p(0), scale: p(1).or(p(0).map(|_| 0)) },
            "real" | "float4" => L::Float { bytes: 4 },
            "double precision" | "float8" => L::Float { bytes: 8 },
            "float" => L::Float { bytes: if p(0).is_some_and(|b| b <= 24) { 4 } else { 8 } },
            // 8-byte count of cents: up to 92 233 720 368 547 758.07, which
            // doesn't fit the (19, 4) of other engines' money.
            "money" => L::Decimal { precision: Some(19), scale: Some(2) },
            // `"char"` (quoted) is the internal one-byte type.
            "\"char\"" => L::Char { len: Some(1), unicode: false },
            "char" | "character" | "bpchar" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "character varying" => match p(0) {
                Some(n) => L::Varchar { len: Some(n), unicode: true },
                None => L::Text { unicode: true },
            },
            "name" => L::Varchar { len: Some(63), unicode: true },
            "text" | "citext" => L::Text { unicode: true },
            "bytea" => L::Blob,
            "bit" => L::Bit { len: p(0).or(Some(1)) },
            "varbit" | "bit varying" => L::Bit { len: p(0) },
            "date" => L::Date,
            "time" => L::Time { precision: frac(), tz: t.with_tz },
            "timetz" => L::Time { precision: frac(), tz: true },
            "timestamp" => L::Timestamp { precision: frac(), tz: t.with_tz },
            "timestamptz" => L::Timestamp { precision: frac(), tz: true },
            // `interval`, `interval(3)`, `interval year to month`,
            // `interval day to second(3)`.
            n if n == "interval" || n.starts_with("interval ") => L::Interval,
            "uuid" => L::Uuid,
            "json" => L::Json { binary: false },
            "jsonb" => L::Json { binary: true },
            "xml" => L::Xml,
            "inet" | "cidr" => L::Inet,
            "macaddr" | "macaddr8" => L::MacAddr,
            "hstore" => L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::Text { unicode: true }) },
            "geometry" | "geography" => L::Geometry {
                kind: t.args.first().map(|k| k.to_ascii_lowercase()),
                srid: p(1),
                geography: t.name == "geography",
            },
            "point" | "line" | "lseg" | "box" | "path" | "polygon" | "circle" => {
                L::Geometry { kind: Some(t.name.clone()), srid: None, geography: false }
            }
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("boolean"),
            L::Int { bytes, unsigned } => {
                let b = L::signed_bytes_for(*bytes, *unsigned);
                let r = match b {
                    1 | 2 => Rendered::exact("smallint"),
                    3 | 4 => Rendered::exact("integer"),
                    8 => Rendered::exact("bigint"),
                    _ => Rendered::exact("numeric(39, 0)"),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, "PostgreSQL no tiene enteros sin signo: se usa un tipo más grande con signo.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } => {
                let s = scale.unwrap_or(0);
                if (*p).max(s) <= MAX_NUMERIC {
                    Rendered::exact(format!("numeric({}, {s})", (*p).max(s)))
                } else {
                    Rendered::exact("numeric")
                        .with(Info, TypeChanged, format!("numeric({p}, {s}) supera la precisión declarable de PostgreSQL (1000): queda numeric sin límite."))
                }
            }
            L::Decimal { precision: None, .. } => Rendered::exact("numeric"),
            L::Float { bytes: 4 } => Rendered::exact("real"),
            L::Float { .. } => Rendered::exact("double precision"),
            L::Money => Rendered::exact("numeric(19, 4)").with(Info, TypeChanged, "Moneda como numeric(19, 4): el tipo money de PostgreSQL depende de la configuración regional."),
            L::Char { len, .. } => match len.unwrap_or(1) {
                n if n <= MAX_VARCHAR => Rendered::exact(format!("char({n})")),
                _ => Rendered::exact("text"),
            },
            L::Varchar { len: Some(n), .. } if *n <= MAX_VARCHAR => Rendered::exact(format!("varchar({n})")),
            L::Varchar { .. } | L::Text { .. } => Rendered::exact("text"),
            L::Binary { len } | L::Varbinary { len } => {
                let r = Rendered::exact("bytea");
                if len.is_some() {
                    r.with(Info, TypeChanged, "bytea no limita el largo.")
                } else {
                    r
                }
            }
            L::Blob => Rendered::exact("bytea"),
            L::Bit { len: Some(n) } => Rendered::exact(format!("bit({n})")),
            L::Bit { len: None } => Rendered::exact("bit varying"),
            L::Date => Rendered::exact("date"),
            L::Time { precision, tz } => Rendered::exact(format!("time{}{}", prec(*precision, 6), if *tz { " with time zone" } else { "" }))
                .with_loss(precision_loss(*precision, 6)),
            L::Timestamp { precision, tz } => {
                Rendered::exact(format!("timestamp{}{}", prec(*precision, 6), if *tz { " with time zone" } else { "" }))
                    .with_loss(precision_loss(*precision, 6))
            }
            L::Interval => Rendered::exact("interval"),
            L::Year => Rendered::exact("smallint").with(Info, TypeChanged, "Año como smallint."),
            L::Uuid => Rendered::exact("uuid"),
            L::Json { binary } => Rendered::exact(if *binary { "jsonb" } else { "json" }),
            L::Xml => Rendered::exact("xml"),
            L::Enum { values } => Rendered::exact(format!("varchar({})", longest(values)))
                .with(Warning, TypeApproximated, format!("PostgreSQL necesita CREATE TYPE para un enumerado: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => Rendered::exact("text[]")
                .with(Warning, TypeApproximated, format!("Conjunto como arreglo de texto. Valores: {}.", values.join(", "))),
            L::Array { of } => {
                let inner = self.render_type(of);
                Rendered { native: format!("{}[]", inner.native), notes: inner.notes }
            }
            L::Map { .. } => Rendered::exact("jsonb").with(Warning, TypeApproximated, "Mapa como jsonb."),
            L::Geometry { kind, srid, geography } => {
                let base = if *geography { "geography" } else { "geometry" };
                let native = match (kind, srid) {
                    (Some(k), Some(s)) => format!("{base}({k}, {s})"),
                    (Some(k), None) => format!("{base}({k})"),
                    _ => base.to_string(),
                };
                Rendered::exact(native).with(Info, TypeChanged, "Requiere la extensión PostGIS.")
            }
            L::Inet => Rendered::exact("inet"),
            L::MacAddr => Rendered::exact("macaddr"),
            L::RowVersion => Rendered::exact("bytea").with(Warning, TypeApproximated, "PostgreSQL no tiene versión de fila automática: queda como bytea y no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        match (d, uuid_slot(ty)) {
            (DefaultValue::NewUuid, UuidSlot::Native) => Some("gen_random_uuid()".into()),
            (DefaultValue::NewUuid, UuidSlot::Text) => Some("(gen_random_uuid())::text".into()),
            (DefaultValue::NewUuid, UuidSlot::Binary) => Some("decode(replace((gen_random_uuid())::text, '-', ''), 'hex')".into()),
            (DefaultValue::NewUuid, UuidSlot::None) => None,
            (DefaultValue::CurrentTimestamp, _) if matches!(ty, L::Date) => Some("CURRENT_DATE".into()),
            (DefaultValue::CurrentTimestamp, _) if matches!(ty, L::Time { .. }) => Some("CURRENT_TIME".into()),
            _ => standard_default(d, ty, "CURRENT_TIMESTAMP", Some("gen_random_uuid()"), false),
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            on_delete: ALL_ACTIONS,
            on_update: ALL_ACTIONS,
            indexes: true,
            partial_indexes: true,
            supports_include: true,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: true,
            max_identifier: 63,
            case: IdentCase::Lower,
        }
    }

    fn implies_auto_increment(&self, t: &TypeSpec) -> bool {
        matches!(t.name.as_str(), "serial" | "serial2" | "serial4" | "serial8" | "smallserial" | "bigserial")
    }

    /// Identity columns must be `smallint`, `integer` or `bigint`.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        for c in t.columns.iter_mut().filter(|c| c.auto_increment) {
            let ok = matches!(self.parse_type(&crate::parse::parse(&c.data_type)), L::Int { bytes: 1..=8, unsigned: false });
            if !ok {
                report.push(
                    Severity::Loss,
                    IssueCode::RangeLoss,
                    &t.name,
                    Some(&c.name),
                    format!("Una columna de identidad de PostgreSQL tiene que ser entera: «{}» pasa a bigint.", c.data_type),
                );
                c.data_type = "bigint".into();
            }
        }
    }
}

/// `(p)` when the precision isn't the engine's default.
pub(crate) fn prec(p: Option<u8>, max: u8) -> String {
    match p {
        Some(p) => format!("({})", p.min(max)),
        None => String::new(),
    }
}

pub(crate) fn precision_loss(p: Option<u8>, max: u8) -> Option<String> {
    p.filter(|p| *p > max).map(|p| format!("Precisión de {p} decimales de segundo reducida a {max}."))
}

pub(crate) fn longest(values: &[String]) -> usize {
    values.iter().map(|v| v.chars().count()).max().unwrap_or(1).max(1)
}

/// Longest value of a SET: every member, comma-separated.
pub(crate) fn set_len(values: &[String]) -> usize {
    (values.iter().map(|v| v.chars().count()).sum::<usize>() + values.len().saturating_sub(1)).max(1)
}

/// `decimal(p, s)` fitted to an engine's limits, keeping the integer digits
/// first (a value that doesn't fit is an error; lost decimals only round).
/// Returns `(precision, scale, lossy)`.
pub(crate) fn fit_decimal(p: u32, s: u32, max_p: u32, max_s: u32) -> (u32, u32, bool) {
    // A scale above the precision (Oracle NUMBER(3, 5)) is 0.00ddd: the
    // same values fit in (s, s).
    let p = p.max(s);
    let int = p - s;
    let int_fit = int.min(max_p);
    let scale = s.min(max_s).min(max_p - int_fit);
    ((int_fit + scale).max(1), scale, int_fit < int || scale < s)
}

/// Where a "new UUID" default can go, by the column's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UuidSlot {
    /// A UUID type.
    Native,
    /// Text of at least 36 characters: the canonical spelling.
    Text,
    /// Bytes of at least 16: the raw value.
    Binary,
    None,
}

pub(crate) fn uuid_slot(ty: &L) -> UuidSlot {
    let fits = |len: &Option<u32>, min: u32| len.is_none_or(|n| n >= min);
    match ty {
        L::Uuid => UuidSlot::Native,
        L::Char { len, .. } | L::Varchar { len, .. } if fits(len, 36) => UuidSlot::Text,
        L::Text { .. } => UuidSlot::Text,
        L::Binary { len } | L::Varbinary { len } if fits(len, 16) => UuidSlot::Binary,
        L::Blob => UuidSlot::Binary,
        _ => UuidSlot::None,
    }
}

/// Keep one auto-increment column (the key's, else the first): engines with
/// one identity per table (SQL Server, Oracle, MySQL).
pub(crate) fn single_auto_increment(t: &mut TableSchema, report: &mut Report, engine: &str) {
    let pk: Vec<String> = t.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default();
    let keep = t
        .columns
        .iter()
        .find(|c| c.auto_increment && pk.contains(&c.name))
        .or_else(|| t.columns.iter().find(|c| c.auto_increment))
        .map(|c| c.name.clone());
    for c in t.columns.iter_mut().filter(|c| c.auto_increment && Some(&c.name) != keep.as_ref()) {
        c.auto_increment = false;
        report.push(
            Severity::Loss,
            IssueCode::AutoIncrementDropped,
            &t.name,
            Some(&c.name),
            format!("{engine} admite una sola columna autoincremental por tabla: esta deja de serlo."),
        );
    }
}

impl Rendered {
    /// Add a precision-loss note when there is one.
    pub(crate) fn with_loss(self, loss: Option<String>) -> Self {
        match loss {
            Some(m) => self.with(Severity::Loss, IssueCode::PrecisionLoss, m),
            None => self,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn ty(s: &str) -> L {
        Postgres.parse_type(&parse(s))
    }

    #[test]
    fn parses_format_type_spellings() {
        assert_eq!(ty("character varying(20)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(ty("character varying"), L::Text { unicode: true });
        assert_eq!(ty("character(10)"), L::Char { len: Some(10), unicode: true });
        assert_eq!(ty("numeric(10,2)"), L::Decimal { precision: Some(10), scale: Some(2) });
        assert_eq!(ty("numeric"), L::Decimal { precision: None, scale: None });
        assert_eq!(ty("timestamp(3) with time zone"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(ty("timestamp without time zone"), L::Timestamp { precision: None, tz: false });
        assert_eq!(ty("time(6) without time zone"), L::Time { precision: Some(6), tz: false });
        assert_eq!(ty("time with time zone"), L::Time { precision: None, tz: true });
        assert_eq!(ty("double precision"), L::Float { bytes: 8 });
        assert_eq!(ty("bit varying(5)"), L::Bit { len: Some(5) });
        assert_eq!(ty("interval"), L::Interval);
        assert_eq!(ty("interval year to month"), L::Interval);
        assert_eq!(ty("interval day to second(3)"), L::Interval);
        assert_eq!(ty("\"char\""), L::Char { len: Some(1), unicode: false });
        assert_eq!(ty("integer GENERATED ALWAYS AS ((a + 1)) STORED"), L::int(4));
        assert_eq!(ty("oid"), L::Int { bytes: 4, unsigned: true });
        assert!(matches!(ty("mood"), L::Other { .. }));
    }

    #[test]
    fn renders_limits() {
        assert_eq!(Postgres.render_type(&L::Decimal { precision: Some(3), scale: Some(5) }).native, "numeric(5, 5)");
        assert_eq!(Postgres.render_type(&L::Decimal { precision: Some(2000), scale: Some(2) }).native, "numeric");
        assert_eq!(Postgres.render_type(&L::Varchar { len: Some(20_000_000), unicode: true }).native, "text");
    }

    #[test]
    fn uuid_defaults_follow_the_column() {
        let d = DefaultValue::NewUuid;
        assert_eq!(Postgres.render_default(&d, &L::Uuid).as_deref(), Some("gen_random_uuid()"));
        assert_eq!(Postgres.render_default(&d, &L::Varchar { len: Some(36), unicode: true }).as_deref(), Some("(gen_random_uuid())::text"));
        assert!(Postgres.render_default(&d, &L::Blob).unwrap().starts_with("decode("));
        assert_eq!(Postgres.render_default(&d, &L::int(4)), None);
    }

    #[test]
    fn fits_decimals_keeping_integer_digits() {
        assert_eq!(fit_decimal(65, 30, 38, 38), (38, 3, true));
        assert_eq!(fit_decimal(10, 2, 38, 38), (10, 2, false));
        assert_eq!(fit_decimal(40, 35, 65, 30), (35, 30, true));
        assert_eq!(fit_decimal(3, 5, 38, 38), (5, 5, false));
        assert_eq!(fit_decimal(50, 0, 38, 38), (38, 0, true));
    }

    #[test]
    fn identity_must_be_an_integer() {
        let mut t = TableSchema {
            name: "t".into(),
            columns: vec![dbine_driver::ColumnDef { name: "id".into(), data_type: "numeric(39, 0)".into(), auto_increment: true, ..Default::default() }],
            ..Default::default()
        };
        let mut r = Report::default();
        Postgres.finalize(&mut t, &mut r);
        assert_eq!(t.columns[0].data_type, "bigint");
        assert_eq!(r.issues[0].code, IssueCode::RangeLoss);
    }
}
