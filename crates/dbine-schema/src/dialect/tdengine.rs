//! TDengine. The first column of every table is its TIMESTAMP key (one row
//! per instant: writing an existing timestamp overwrites the row); there
//! are no primary keys, NOT NULL, defaults, indexes, foreign keys nor
//! column comments. Supertables add tag columns (`tag` column option).
//! Strings are VARCHAR (bytes) or NCHAR (unicode characters) with a
//! mandatory length.

use super::greptimedb::time_column;
use super::postgres::longest;
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{ColumnDef, TableSchema};

pub struct TDengine;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: TDengine = TDengine;
    (driver_id == "tdengine").then_some(&D as &dyn Dialect)
}

/// Longest VARCHAR / VARBINARY (bytes) and NCHAR (characters).
const MAX_VARCHAR: u32 = 65_517;
const MAX_NCHAR: u32 = 16_379;
/// Length given to unbounded text: rows are limited to 64 KB in all.
const TEXT_LEN: u32 = 4_096;
/// The note on dates rendered as TIMESTAMP; `finalize` reads it back so a
/// date isn't taken for the table's time axis.
const DATE_NOTE: &str = "TDengine no tiene fechas: queda como marca de tiempo a medianoche.";
/// Column added when a table has no timestamp.
pub const TS: &str = "ts";

impl Dialect for TDengine {
    fn id(&self) -> &'static str {
        "tdengine"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let u = t.unsigned;
        match t.name.as_str() {
            // Epoch-based, in the database's precision (ms by default).
            "timestamp" => L::Timestamp { precision: None, tz: true },
            "bool" => L::Bool,
            "tinyint" => L::Int { bytes: 1, unsigned: u },
            "smallint" => L::Int { bytes: 2, unsigned: u },
            "int" | "integer" => L::Int { bytes: 4, unsigned: u },
            "bigint" => L::Int { bytes: 8, unsigned: u },
            "utinyint" => L::Int { bytes: 1, unsigned: true },
            "usmallint" => L::Int { bytes: 2, unsigned: true },
            "uint" => L::Int { bytes: 4, unsigned: true },
            "ubigint" => L::Int { bytes: 8, unsigned: true },
            "float" => L::Float { bytes: 4 },
            "double" => L::Float { bytes: 8 },
            "decimal" => L::Decimal { precision: p(0).or(Some(10)), scale: p(1).or(Some(0)) },
            "varchar" | "binary" => L::Varchar { len: p(0), unicode: false },
            "nchar" => L::Varchar { len: p(0), unicode: true },
            "varbinary" => L::Varbinary { len: p(0) },
            "blob" => L::Blob,
            "geometry" => L::Geometry { kind: None, srid: None, geography: false },
            "json" => L::Json { binary: false },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let nchar = |why: String| Rendered::exact(format!("NCHAR({TEXT_LEN})")).with(Warning, TypeApproximated, why);
        match t {
            L::Bool => Rendered::exact("BOOL"),
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
            L::Decimal { precision: Some(p), scale } => super::starrocks::capped_decimal(*p, *scale, 38, "TDengine"),
            L::Decimal { precision: None, .. } => super::starrocks::unbounded_decimal("TDengine"),
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(Info, TypeChanged, "Moneda como DECIMAL(19, 4)."),
            L::Char { len, unicode } => sized(len.unwrap_or(1), *unicode)
                .with(Info, TypeChanged, "TDengine no tiene texto de largo fijo: sin relleno de espacios."),
            L::Varchar { len: Some(n), unicode } => sized(*n, *unicode),
            L::Varchar { unicode, .. } | L::Text { unicode } => {
                let native = if *unicode { format!("NCHAR({TEXT_LEN})") } else { format!("VARCHAR({TEXT_LEN})") };
                Rendered::exact(native).with(Loss, LengthLoss, format!("TDengine exige un largo: se usan {TEXT_LEN} caracteres (las filas no pueden pasar de 64 KB)."))
            }
            L::Binary { len: Some(n) } | L::Varbinary { len: Some(n) } if *n <= MAX_VARCHAR => Rendered::exact(format!("VARBINARY({n})")),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact(format!("VARBINARY({MAX_VARCHAR})"))
                .with(Loss, LengthLoss, format!("VARBINARY de TDengine admite hasta {MAX_VARCHAR} bytes.")),
            L::Bit { len } => match len {
                Some(n) if *n <= 64 => Rendered::exact("BIGINT UNSIGNED").with(Info, TypeChanged, "Cadena de bits como entero."),
                _ => Rendered::exact(format!("VARBINARY({})", len.unwrap_or(64).div_ceil(8).max(1)))
                    .with(Warning, TypeApproximated, "Cadena de bits larga: se guarda como binario."),
            },
            L::Date => Rendered::exact("TIMESTAMP").with(Info, TypeChanged, DATE_NOTE),
            L::Time { tz, .. } => {
                let r = Rendered::exact("VARCHAR(32)").with(Warning, TypeApproximated, "TDengine no tiene horas: queda como texto HH:MM:SS.");
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Se pierde la zona horaria de la hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let mut r = Rendered::exact("TIMESTAMP");
                if precision.is_none_or(|p| p > 3) {
                    r = r.with(Warning, PrecisionLoss, "TIMESTAMP guarda milisegundos salvo que la base se cree con PRECISION 'us' o 'ns'.");
                }
                if !tz {
                    r = r.with(Info, TimeZoneLoss, "TDengine guarda instantes: los valores sin zona se interpretan en la zona del cliente.");
                }
                r
            }
            L::Interval => Rendered::exact("VARCHAR(64)").with(Warning, TypeApproximated, "TDengine no tiene intervalos: queda como texto."),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            L::Uuid => Rendered::exact("VARCHAR(36)").with(Info, TypeChanged, "UUID como VARCHAR(36)."),
            // JSON only exists as the single tag of a supertable.
            L::Json { .. } => nchar("En TDengine JSON solo se admite como tag de una supertabla: queda como texto.".into()),
            L::Xml => nchar("XML como texto.".into()),
            L::Enum { values } => Rendered::exact(format!("NCHAR({})", longest(values)))
                .with(Warning, TypeApproximated, format!("TDengine no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => nchar(format!("Conjunto como texto. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } => nchar("TDengine no tiene arreglos ni mapas: se guarda como texto JSON.".into()),
            L::Geometry { .. } => Rendered::exact(format!("VARCHAR({TEXT_LEN})")).with(Warning, TypeApproximated, "Dato espacial como texto (WKT)."),
            L::Inet => Rendered::exact("VARCHAR(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("BIGINT").with(Warning, TypeApproximated, "TDengine no tiene versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, _d: &DefaultValue, _ty: &L) -> Option<String> {
        None
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
            defaults: false,
            nullability: false,
            // Table comments only; column comments are dropped in finalize.
            comments: true,
            max_identifier: 64,
            case: IdentCase::Lower,
        }
    }

    /// The timestamp key first: the key's timestamp (or the first one)
    /// moves to the front; a table without one gets `ts`.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        use IssueCode::*;
        use Severity::*;
        let table = t.name.clone();
        let is_ts = |c: &ColumnDef| c.data_type.eq_ignore_ascii_case("TIMESTAMP");
        let tag = |c: &ColumnDef| c.options.get("tag").is_some_and(|v| v == "true" || v == "1");
        if !t.columns.first().is_some_and(|c| is_ts(c) && !tag(c)) {
            // Dates became TIMESTAMP too: they don't count as the time axis.
            let mut instants = t.clone();
            for c in instants.columns.iter_mut() {
                if report.issues.iter().any(|i| i.object.as_deref() == Some(&c.name) && i.message == DATE_NOTE) {
                    c.data_type = "DATE".into();
                }
            }
            match time_column(&instants, |s| s.name == "timestamp") {
                Some(i) => {
                    let c = t.columns.remove(i);
                    report.push(Warning, PrimaryKeyAdded, &table, Some(&c.name), format!(
                        "«{}» pasa a ser la primera columna: en TDengine la marca de tiempo es la clave (una fila por instante; repetir uno sobrescribe la fila).", c.name
                    ));
                    t.columns.insert(0, c);
                }
                None => {
                    let name = if t.columns.iter().any(|c| c.name.eq_ignore_ascii_case(TS)) { format!("{TS}_1") } else { TS.to_string() };
                    t.columns.insert(0, ColumnDef { name: name.clone(), data_type: "TIMESTAMP".into(), nullable: false, ..Default::default() });
                    report.push(Warning, PrimaryKeyAdded, &table, Some(&name), format!(
                        "TDengine exige una primera columna TIMESTAMP: se agrega «{name}». Al copiar hay que darle un valor distinto por fila (repetir uno sobrescribe la fila)."
                    ));
                }
            }
        }
        let ts = t.columns[0].name.clone();
        if let Some(k) = t.primary_key.take().filter(|k| k.columns != [ts.clone()]) {
            report.push(Warning, PrimaryKeyDropped, &table, Some(&k.columns.join(", ")), format!(
                "TDengine no tiene claves primarias: la unicidad la da «{ts}»."
            ));
        }
        for c in t.columns.iter_mut().filter(|c| c.comment.is_some()) {
            c.comment = None;
            report.push(Info, CommentDropped, &table, Some(&c.name), "TDengine no guarda comentarios de columna.");
        }
    }
}

/// Text of `n` characters: NCHAR when unicode, VARCHAR (bytes) when not.
fn sized(n: u32, unicode: bool) -> Rendered {
    use IssueCode::*;
    use Severity::*;
    if unicode {
        if n <= MAX_NCHAR {
            Rendered::exact(format!("NCHAR({n})"))
        } else {
            Rendered::exact(format!("NCHAR({MAX_NCHAR})")).with(Loss, LengthLoss, format!("NCHAR de TDengine admite hasta {MAX_NCHAR} caracteres."))
        }
    } else if n <= MAX_VARCHAR {
        Rendered::exact(format!("VARCHAR({n})"))
    } else {
        Rendered::exact(format!("VARCHAR({MAX_VARCHAR})")).with(Loss, LengthLoss, format!("VARCHAR de TDengine admite hasta {MAX_VARCHAR} bytes."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use dbine_driver::KeyDef;

    fn p(s: &str) -> L {
        crate::convert::logical_of(&TDengine, &parse(s))
    }

    #[test]
    fn parses_describe_types() {
        assert_eq!(p("TIMESTAMP"), L::Timestamp { precision: None, tz: true });
        assert_eq!(p("BOOL"), L::Bool);
        assert_eq!(p("TINYINT UNSIGNED"), L::Int { bytes: 1, unsigned: true });
        assert_eq!(p("INT"), L::int(4));
        assert_eq!(p("BIGINT UNSIGNED"), L::Int { bytes: 8, unsigned: true });
        assert_eq!(p("FLOAT"), L::Float { bytes: 4 });
        assert_eq!(p("DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(p("DECIMAL(18, 2)"), L::Decimal { precision: Some(18), scale: Some(2) });
        assert_eq!(p("VARCHAR(20)"), L::Varchar { len: Some(20), unicode: false });
        assert_eq!(p("BINARY(20)"), L::Varchar { len: Some(20), unicode: false });
        assert_eq!(p("NCHAR(10)"), L::Varchar { len: Some(10), unicode: true });
        assert_eq!(p("VARBINARY(16)"), L::Varbinary { len: Some(16) });
        assert_eq!(p("GEOMETRY(64)"), L::Geometry { kind: None, srid: None, geography: false });
        assert_eq!(p("JSON"), L::Json { binary: false });
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| TDengine.render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOL");
        assert_eq!(r(L::Int { bytes: 2, unsigned: true }), "SMALLINT UNSIGNED");
        assert_eq!(r(L::int(3)), "INT");
        assert_eq!(r(L::int(16)), "DECIMAL(38, 0)");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "DECIMAL(12, 2)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "DECIMAL(38, 10)");
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Float { bytes: 8 }), "DOUBLE");
        assert_eq!(r(L::Money), "DECIMAL(19, 4)");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }), "NCHAR(3)");
        assert_eq!(r(L::Char { len: Some(3), unicode: false }), "VARCHAR(3)");
        assert_eq!(r(L::Varchar { len: Some(20), unicode: true }), "NCHAR(20)");
        assert_eq!(r(L::Varchar { len: Some(90_000), unicode: false }), "VARCHAR(65517)");
        assert_eq!(r(L::Varchar { len: None, unicode: true }), "NCHAR(4096)");
        assert_eq!(r(L::Text { unicode: true }), "NCHAR(4096)");
        assert_eq!(r(L::Text { unicode: false }), "VARCHAR(4096)");
        assert_eq!(r(L::Binary { len: Some(16) }), "VARBINARY(16)");
        assert_eq!(r(L::Blob), "VARBINARY(65517)");
        assert_eq!(r(L::Bit { len: Some(8) }), "BIGINT UNSIGNED");
        assert_eq!(r(L::Date), "TIMESTAMP");
        assert_eq!(r(L::Time { precision: None, tz: false }), "VARCHAR(32)");
        assert_eq!(r(L::Timestamp { precision: Some(3), tz: true }), "TIMESTAMP");
        assert_eq!(r(L::Interval), "VARCHAR(64)");
        assert_eq!(r(L::Year), "SMALLINT");
        assert_eq!(r(L::Uuid), "VARCHAR(36)");
        assert_eq!(r(L::Json { binary: true }), "NCHAR(4096)");
        assert_eq!(r(L::Xml), "NCHAR(4096)");
        assert_eq!(r(L::Enum { values: vec!["ab".into()] }), "NCHAR(2)");
        assert_eq!(r(L::Set { values: vec![] }), "NCHAR(4096)");
        assert_eq!(r(L::Array { of: Box::new(L::Bool) }), "NCHAR(4096)");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "NCHAR(4096)");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "VARCHAR(4096)");
        assert_eq!(r(L::Inet), "VARCHAR(45)");
        assert_eq!(r(L::MacAddr), "VARCHAR(17)");
        assert_eq!(r(L::RowVersion), "BIGINT");
    }

    #[test]
    fn finalize_puts_a_timestamp_first() {
        let col = |n: &str, ty: &str| ColumnDef { name: n.into(), data_type: ty.into(), comment: Some("c".into()), ..Default::default() };
        let mut t = TableSchema {
            name: "t".into(),
            columns: vec![col("id", "BIGINT"), col("alta", "TIMESTAMP")],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            ..Default::default()
        };
        let mut rep = Report::default();
        TDengine.finalize(&mut t, &mut rep);
        assert_eq!(t.columns[0].name, "alta");
        assert!(t.primary_key.is_none());
        assert!(t.columns.iter().all(|c| c.comment.is_none()));

        let mut t = TableSchema { name: "t".into(), columns: vec![col("ts", "INT")], ..Default::default() };
        let mut rep = Report::default();
        TDengine.finalize(&mut t, &mut rep);
        assert_eq!((t.columns[0].name.as_str(), t.columns[0].data_type.as_str()), ("ts_1", "TIMESTAMP"));
    }
}
