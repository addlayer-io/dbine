//! SQLite (and libSQL). SQLite stores values by *affinity*, not by the
//! declared type, and doesn't enforce lengths: parsing keeps the declared
//! intent (a `DATETIME` column holds dates), rendering keeps names other
//! engines and people recognize (and sizes, so a round trip through SQLite
//! gives the same types back).
//!
//! Types arrive as declared in CREATE TABLE (`pragma_table_xinfo.type`),
//! any case and spelling: `VARCHAR(20)`, `UNSIGNED BIG INT`, `NUMERIC(10, 2)`.

use super::postgres::{uuid_slot, UuidSlot};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered, ALL_ACTIONS};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{parse, TypeSpec};
use dbine_driver::TableSchema;

pub struct Sqlite;

impl Dialect for Sqlite {
    fn id(&self) -> &'static str {
        "sqlite"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        // `UNSIGNED BIG INT` starts with a modifier: the whole spelling decides.
        let raw = t.raw.to_ascii_lowercase();
        let n = if t.name.is_empty() && !raw.is_empty() {
            raw.as_str()
        } else {
            t.name.as_str()
        };
        let int = |bytes| L::Int {
            bytes,
            unsigned: t.unsigned,
        };
        match n {
            "boolean" | "bool" => L::Bool,
            "tinyint" => int(1),
            "smallint" | "int2" => int(2),
            "mediumint" => int(3),
            "int" | "int4" => int(4),
            // INTEGER is SQLite's 8-byte integer (the rowid).
            "integer" | "bigint" | "int8" => int(8),
            "unsigned big int" => L::Int {
                bytes: 8,
                unsigned: true,
            },
            "real" | "double" | "double precision" | "float" => L::Float { bytes: 8 },
            "numeric" | "decimal" => L::Decimal {
                precision: p(0),
                scale: p(1).or(p(0).map(|_| 0)),
            },
            "char" | "character" | "nchar" | "native character" => L::Char {
                len: p(0),
                unicode: true,
            },
            "varchar" | "nvarchar" | "varying character" | "character varying" => L::Varchar {
                len: p(0),
                unicode: true,
            },
            "text" | "clob" => L::Text { unicode: true },
            "blob" => L::Blob,
            // No type: BLOB affinity, stores anything as given.
            "" => L::Blob,
            "date" => L::Date,
            "time" => L::Time {
                precision: None,
                tz: false,
            },
            "datetime" | "timestamp" => L::Timestamp {
                precision: None,
                tz: false,
            },
            "uuid" => L::Uuid,
            "json" => L::Json { binary: false },
            // Affinity rules (https://sqlite.org/datatype3.html §3.1).
            _ if n.contains("int") => int(8),
            _ if n.contains("char") || n.contains("clob") || n.contains("text") => {
                L::Text { unicode: true }
            }
            _ if n.contains("blob") => L::Blob,
            _ if n.contains("real") || n.contains("floa") || n.contains("doub") => {
                L::Float { bytes: 8 }
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
            L::Bool => Rendered::exact("BOOLEAN"),
            // All are 8-byte INTEGER affinity; the name keeps the size.
            L::Int { bytes, unsigned } => match L::signed_bytes_for(*bytes, *unsigned) {
                1 => Rendered::exact("TINYINT"),
                2 => Rendered::exact("SMALLINT"),
                3 => Rendered::exact("MEDIUMINT"),
                4 => Rendered::exact("INT"),
                8 => Rendered::exact("BIGINT"),
                _ => Rendered::exact("NUMERIC").with(
                    Loss,
                    RangeLoss,
                    "SQLite guarda enteros de hasta 8 bytes; los mayores quedan como número real.",
                ),
            },
            L::Decimal { precision, scale } => {
                let native = match (precision, scale) {
                    (Some(p), Some(s)) => format!("NUMERIC({p}, {s})"),
                    (Some(p), None) => format!("NUMERIC({p})"),
                    _ => "NUMERIC".into(),
                };
                let r = Rendered::exact(native);
                if precision.is_none_or(|p| p > 15) || scale.is_some_and(|s| s > 0) {
                    r.with(Loss, PrecisionLoss, "SQLite guarda los decimales como número real (unos 15 dígitos): puede redondear.")
                } else {
                    r
                }
            }
            L::Float { .. } => Rendered::exact("REAL"),
            L::Money => Rendered::exact("NUMERIC").with(
                Loss,
                PrecisionLoss,
                "SQLite guarda la moneda como número real: puede redondear.",
            ),
            L::Char { len, .. } => {
                Rendered::exact(len.map_or("CHAR".into(), |n| format!("CHAR({n})")))
            }
            L::Varchar { len: Some(n), .. } => Rendered::exact(format!("VARCHAR({n})")),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact("TEXT"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("BLOB"),
            L::Bit { .. } => Rendered::exact("BLOB").with(
                Warning,
                TypeApproximated,
                "SQLite no tiene cadenas de bits: se guarda como binario.",
            ),
            L::Date => Rendered::exact("DATE"),
            L::Time { tz, .. } => {
                let r = Rendered::exact("TIME");
                if *tz {
                    r.with(Loss, TimeZoneLoss, "SQLite no guarda zonas horarias.")
                } else {
                    r
                }
            }
            L::Timestamp { tz, .. } => {
                let r = Rendered::exact("DATETIME");
                if *tz {
                    r.with(
                        Loss,
                        TimeZoneLoss,
                        "SQLite no guarda zonas horarias: conviene guardar en UTC.",
                    )
                } else {
                    r
                }
            }
            L::Interval => {
                Rendered::exact("TEXT").with(Warning, TypeApproximated, "Intervalo como texto.")
            }
            L::Year => Rendered::exact("INTEGER"),
            L::Uuid => Rendered::exact("TEXT").with(Info, TypeChanged, "UUID como texto."),
            L::Json { .. } => Rendered::exact("TEXT").with(
                Info,
                TypeChanged,
                "JSON como texto (las funciones json_* de SQLite lo leen).",
            ),
            L::Xml => Rendered::exact("TEXT"),
            L::Enum { values } | L::Set { values } => Rendered::exact("TEXT").with(
                Warning,
                TypeApproximated,
                format!(
                    "SQLite no tiene enumerados: queda como texto. Valores: {}.",
                    values.join(", ")
                ),
            ),
            L::Array { .. } | L::Map { .. } => Rendered::exact("TEXT").with(
                Warning,
                TypeApproximated,
                "SQLite no tiene arreglos ni mapas: se guarda como JSON en texto.",
            ),
            L::Geometry { .. } => Rendered::exact("BLOB").with(
                Warning,
                TypeApproximated,
                "Dato espacial como binario (SpatiaLite lo interpreta).",
            ),
            L::Inet | L::MacAddr => Rendered::exact("TEXT"),
            L::RowVersion => Rendered::exact("BLOB").with(
                Warning,
                TypeApproximated,
                "SQLite no tiene versión de fila automática.",
            ),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            // No UUID function: a random version-4 one from randomblob().
            DefaultValue::NewUuid => match uuid_slot(ty) {
                UuidSlot::Native | UuidSlot::Text => Some(
                    "(lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || \
                     substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6))))"
                        .into(),
                ),
                UuidSlot::Binary => Some("(randomblob(16))".into()),
                UuidSlot::None => None,
            },
            DefaultValue::CurrentTimestamp if matches!(ty, L::Date) => Some("CURRENT_DATE".into()),
            DefaultValue::CurrentTimestamp if matches!(ty, L::Time { .. }) => Some("CURRENT_TIME".into()),
            _ => standard_default(d, ty, "CURRENT_TIMESTAMP", None, true),
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            on_delete: ALL_ACTIONS,
            on_update: ALL_ACTIONS,
            indexes: true,
            partial_indexes: true,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: false,
            max_identifier: 1024,
            case: IdentCase::Preserve,
        }
    }

    /// AUTOINCREMENT only exists on an `INTEGER PRIMARY KEY` of one column:
    /// an integer key keeps it as `INTEGER` (SQLite's integers are all 8
    /// bytes); anything else loses it.
    fn finalize(&self, t: &mut TableSchema, report: &mut crate::issue::Report) {
        let pk: Vec<String> = t
            .primary_key
            .as_ref()
            .map(|k| k.columns.clone())
            .unwrap_or_default();
        for c in &mut t.columns {
            if !c.auto_increment {
                continue;
            }
            // A key of any whole-number type counts (MySQL's BIGINT UNSIGNED,
            // Oracle's NUMBER(19) / INTEGER): the rowid goes up to 2^63 - 1.
            let logical = self.parse_type(&parse(&c.data_type));
            let whole = matches!(
                logical,
                L::Int { .. }
                    | L::Decimal {
                        scale: Some(0) | None,
                        ..
                    }
            );
            if pk.len() == 1 && pk[0] == c.name && whole {
                if !matches!(
                    logical,
                    L::Int {
                        bytes: 1..=8,
                        unsigned: false
                    }
                ) {
                    report.push(
                        Severity::Loss,
                        IssueCode::RangeLoss,
                        &t.name,
                        Some(&c.name),
                        format!("La clave autoincremental de SQLite es INTEGER (hasta 2^63 - 1): «{}» pasa a INTEGER.", c.data_type),
                    );
                }
                c.data_type = "INTEGER".into();
            } else {
                c.auto_increment = false;
                report.push(
                    Severity::Loss,
                    IssueCode::AutoIncrementDropped,
                    &t.name,
                    Some(&c.name),
                    "En SQLite solo una clave primaria INTEGER de una columna es autoincremental.",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(s: &str) -> L {
        Sqlite.parse_type(&parse(s))
    }

    #[test]
    fn parses_declared_types() {
        assert_eq!(ty("INTEGER"), L::int(8));
        assert_eq!(ty("INT"), L::int(4));
        assert_eq!(
            ty("UNSIGNED BIG INT"),
            L::Int {
                bytes: 8,
                unsigned: true
            }
        );
        assert_eq!(
            ty("INT UNSIGNED"),
            L::Int {
                bytes: 4,
                unsigned: true
            }
        );
        assert_eq!(
            ty("NUMERIC(10, 2)"),
            L::Decimal {
                precision: Some(10),
                scale: Some(2)
            }
        );
        assert_eq!(
            ty("DECIMAL(10)"),
            L::Decimal {
                precision: Some(10),
                scale: Some(0)
            }
        );
        assert_eq!(
            ty("VARCHAR(20)"),
            L::Varchar {
                len: Some(20),
                unicode: true
            }
        );
        assert_eq!(
            ty("NATIVE CHARACTER(70)"),
            L::Char {
                len: Some(70),
                unicode: true
            }
        );
        assert_eq!(
            ty("DATETIME"),
            L::Timestamp {
                precision: None,
                tz: false
            }
        );
        assert_eq!(ty(""), L::Blob);
        assert_eq!(
            ty("STRING"),
            L::Other {
                native: "STRING".into()
            }
        );
        assert_eq!(ty("VARCHAR2(10)"), L::Text { unicode: true });
    }

    #[test]
    fn round_trips_integer_sizes() {
        for b in [1u8, 2, 3, 4, 8] {
            let r = Sqlite.render_type(&L::int(b));
            assert_eq!(ty(&r.native), L::int(b));
        }
    }

    /// The native name and the worst note's severity (None: exact).
    fn render(t: L) -> (String, Option<Severity>) {
        let r = Sqlite.render_type(&t);
        (r.native, r.notes.iter().map(|n| n.severity).max())
    }

    #[test]
    fn renders_every_type_and_says_what_it_loses() {
        use Severity::*;
        let cases = [
            (L::Bool, "BOOLEAN", None),
            (
                L::Int {
                    bytes: 8,
                    unsigned: true,
                },
                "NUMERIC",
                Some(Loss),
            ),
            (
                L::Int {
                    bytes: 4,
                    unsigned: true,
                },
                "BIGINT",
                None,
            ),
            (
                L::Decimal {
                    precision: Some(10),
                    scale: Some(2),
                },
                "NUMERIC(10, 2)",
                Some(Loss),
            ),
            (
                L::Decimal {
                    precision: Some(9),
                    scale: None,
                },
                "NUMERIC(9)",
                None,
            ),
            (
                L::Decimal {
                    precision: None,
                    scale: None,
                },
                "NUMERIC",
                Some(Loss),
            ),
            (L::Float { bytes: 4 }, "REAL", None),
            (L::Money, "NUMERIC", Some(Loss)),
            (
                L::Char {
                    len: None,
                    unicode: false,
                },
                "CHAR",
                None,
            ),
            (
                L::Char {
                    len: Some(2),
                    unicode: false,
                },
                "CHAR(2)",
                None,
            ),
            (
                L::Varchar {
                    len: Some(9),
                    unicode: true,
                },
                "VARCHAR(9)",
                None,
            ),
            (
                L::Varchar {
                    len: None,
                    unicode: true,
                },
                "TEXT",
                None,
            ),
            (L::Varbinary { len: Some(4) }, "BLOB", None),
            (L::Bit { len: Some(8) }, "BLOB", Some(Warning)),
            (L::Date, "DATE", None),
            (
                L::Time {
                    precision: None,
                    tz: false,
                },
                "TIME",
                None,
            ),
            (
                L::Time {
                    precision: None,
                    tz: true,
                },
                "TIME",
                Some(Loss),
            ),
            (
                L::Timestamp {
                    precision: Some(3),
                    tz: true,
                },
                "DATETIME",
                Some(Loss),
            ),
            (L::Interval, "TEXT", Some(Warning)),
            (L::Year, "INTEGER", None),
            (L::Uuid, "TEXT", Some(Info)),
            (L::Json { binary: true }, "TEXT", Some(Info)),
            (L::Xml, "TEXT", None),
            (
                L::Set {
                    values: vec!["a".into(), "b".into()],
                },
                "TEXT",
                Some(Warning),
            ),
            (
                L::Map {
                    key: Box::new(L::Uuid),
                    value: Box::new(L::Bool),
                },
                "TEXT",
                Some(Warning),
            ),
            (
                L::Geometry {
                    kind: None,
                    srid: None,
                    geography: false,
                },
                "BLOB",
                Some(Warning),
            ),
            (L::MacAddr, "TEXT", None),
            (L::RowVersion, "BLOB", Some(Warning)),
            (
                L::Other {
                    native: "STRING".into(),
                },
                "STRING",
                None,
            ),
        ];
        for (t, native, worst) in cases {
            let got = render(t.clone());
            assert_eq!(got, (native.to_string(), worst), "{t:?}");
        }
        // An enum's values are listed in its note.
        let r = Sqlite.render_type(&L::Enum {
            values: vec!["rojo".into(), "verde".into()],
        });
        assert!(r.notes[0].message.contains("rojo, verde"), "{:?}", r.notes);
    }

    #[test]
    fn defaults_sqlite_can_compute() {
        let uuid_text = Sqlite
            .render_default(&DefaultValue::NewUuid, &L::Uuid)
            .unwrap();
        assert!(uuid_text.contains("randomblob"), "{uuid_text}");
        assert_eq!(
            Sqlite.render_default(
                &DefaultValue::NewUuid,
                &L::Varchar {
                    len: Some(36),
                    unicode: false
                }
            ),
            Some(uuid_text)
        );
        assert_eq!(
            Sqlite
                .render_default(&DefaultValue::NewUuid, &L::Binary { len: Some(16) })
                .as_deref(),
            Some("(randomblob(16))")
        );
        // Too short to hold one: no default.
        assert_eq!(
            Sqlite.render_default(
                &DefaultValue::NewUuid,
                &L::Varchar {
                    len: Some(10),
                    unicode: false
                }
            ),
            None
        );
        assert_eq!(
            Sqlite
                .render_default(&DefaultValue::CurrentTimestamp, &L::Date)
                .as_deref(),
            Some("CURRENT_DATE")
        );
        assert_eq!(
            Sqlite
                .render_default(
                    &DefaultValue::CurrentTimestamp,
                    &L::Time {
                        precision: None,
                        tz: false
                    }
                )
                .as_deref(),
            Some("CURRENT_TIME")
        );
        assert_eq!(
            Sqlite
                .render_default(
                    &DefaultValue::CurrentTimestamp,
                    &L::Timestamp {
                        precision: None,
                        tz: false
                    }
                )
                .as_deref(),
            Some("CURRENT_TIMESTAMP")
        );
        let caps = Sqlite.caps();
        assert!(caps.partial_indexes && !caps.supports_include && !caps.comments);
    }

    fn table(cols: &[(&str, &str, bool)], pk: &[&str]) -> TableSchema {
        TableSchema {
            name: "t".into(),
            columns: cols
                .iter()
                .map(|(n, t, auto)| dbine_driver::ColumnDef {
                    name: n.to_string(),
                    data_type: t.to_string(),
                    auto_increment: *auto,
                    ..Default::default()
                })
                .collect(),
            primary_key: (!pk.is_empty()).then(|| dbine_driver::KeyDef {
                name: None,
                columns: pk.iter().map(|s| s.to_string()).collect(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn only_an_integer_key_of_one_column_auto_increments() {
        // A key of a whole-number type: INTEGER, quietly.
        let mut t = table(&[("id", "BIGINT", true)], &["id"]);
        let mut report = crate::issue::Report::default();
        Sqlite.finalize(&mut t, &mut report);
        assert_eq!(t.columns[0].data_type, "INTEGER");
        assert!(t.columns[0].auto_increment);
        assert!(report.issues.is_empty());
        // Unsigned or wider than 8 bytes: INTEGER, with the range noted.
        let mut t = table(&[("id", "NUMERIC(38)", true)], &["id"]);
        Sqlite.finalize(&mut t, &mut report);
        assert_eq!(t.columns[0].data_type, "INTEGER");
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].code, IssueCode::RangeLoss);
        // Not the key, a key of two columns, or not a whole number: dropped.
        for mut t in [
            table(&[("id", "INT", true), ("k", "INT", false)], &["k"]),
            table(&[("id", "INT", true), ("k", "INT", false)], &["id", "k"]),
            table(&[("id", "NUMERIC(10, 2)", true)], &["id"]),
        ] {
            let mut report = crate::issue::Report::default();
            Sqlite.finalize(&mut t, &mut report);
            assert!(!t.columns[0].auto_increment);
            assert_eq!(report.issues[0].code, IssueCode::AutoIncrementDropped);
        }
    }
}
