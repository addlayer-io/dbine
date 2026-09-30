//! IBM Db2: LUW (Linux, UNIX, Windows), Db2 for i (AS/400) and Db2 for
//! z/OS. The three share the type names; they differ in limits (CHAR,
//! VARCHAR, DECIMAL), in BOOLEAN (not on z/OS), in time zones (only z/OS
//! has `TIMESTAMP WITH TIME ZONE`) and in the default encoding (LUW
//! databases are UTF-8 since 9.5, i and z/OS default to EBCDIC).
//!
//! The ODBC driver (IBM Data Server / IBM i Access) reports the names in
//! upper case with the length in parentheses (`VARCHAR(40)`,
//! `DECIMAL(12,2)`), binary strings as `CHAR () FOR BIT DATA`, and
//! timestamps without their precision.

use super::postgres::{longest, precision_loss, prec};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Luw,
    /// Db2 for i.
    I,
    Zos,
}

pub struct Db2 {
    pub platform: Platform,
}

/// Limits per platform, in bytes unless said otherwise.
struct Limits {
    char: u32,
    varchar: u32,
    binary: u32,
    varbinary: u32,
    decimal: u32,
    /// GRAPHIC / NCHAR, in double-byte characters.
    graphic: u32,
    /// VARGRAPHIC / NVARCHAR, in double-byte characters.
    vargraphic: u32,
}

/// `VARCHAR(n CODEUNITS32)` on LUW: 32672 bytes / 4.
const LUW_VARCHAR_CU32: u32 = 8168;
const LUW_CHAR_CU32: u32 = 63;
/// Largest LOB that doesn't have to be NOT LOGGED on LUW.
const CLOB: &str = "CLOB(1G)";
const BLOB: &str = "BLOB(1G)";
const TIMESTAMP_MAX: u8 = 12;

impl Db2 {
    fn limits(&self) -> Limits {
        match self.platform {
            Platform::Luw => Limits { char: 254, varchar: 32672, binary: 255, varbinary: 32672, decimal: 31, graphic: 127, vargraphic: 16336 },
            Platform::I => Limits { char: 32765, varchar: 32739, binary: 32765, varbinary: 32739, decimal: 63, graphic: 16382, vargraphic: 16369 },
            Platform::Zos => Limits { char: 255, varchar: 32704, binary: 255, varbinary: 32704, decimal: 31, graphic: 127, vargraphic: 16352 },
        }
    }

    fn name(&self) -> &'static str {
        match self.platform {
            Platform::Luw => "Db2",
            Platform::I => "Db2 for i",
            Platform::Zos => "Db2 for z/OS",
        }
    }

    /// Unicode text that doesn't fit a VARCHAR/VARGRAPHIC.
    fn unicode_lob(&self) -> &'static str {
        match self.platform {
            // UTF-8 database: a CLOB holds any character.
            Platform::Luw => CLOB,
            Platform::I => "NCLOB(512M)",
            Platform::Zos => "DBCLOB(512M)",
        }
    }

    fn text(&self, len: Option<u32>, unicode: bool, fixed: bool) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let lim = self.limits();
        let n = len.unwrap_or(1);
        if !unicode {
            return match len {
                Some(n) if fixed && n <= lim.char => Rendered::exact(format!("CHAR({n})")),
                Some(n) if n <= lim.varchar => {
                    let r = Rendered::exact(format!("VARCHAR({n})"));
                    if fixed {
                        r.with(Info, TypeChanged, format!("CHAR({n}) supera el máximo de {} en {}: se usa VARCHAR.", lim.char, self.name()))
                    } else {
                        r
                    }
                }
                Some(n) => Rendered::exact(CLOB).with(Info, TypeChanged, format!("{n} caracteres superan el VARCHAR de {}: se usa CLOB.", self.name())),
                None if fixed => Rendered::exact("CHAR(1)"),
                None => Rendered::exact(CLOB),
            };
        }
        let lob = |why: String| Rendered::exact(self.unicode_lob()).with(Info, TypeChanged, why);
        match self.platform {
            // UTF-8 database: CODEUNITS32 counts characters, not bytes.
            Platform::Luw => match len {
                Some(n) if fixed && n <= LUW_CHAR_CU32 => Rendered::exact(format!("CHAR({n} CODEUNITS32)")),
                Some(n) if n <= LUW_VARCHAR_CU32 => Rendered::exact(format!("VARCHAR({n} CODEUNITS32)")),
                Some(n) => lob(format!("{n} caracteres Unicode superan los {LUW_VARCHAR_CU32} de VARCHAR: se usa CLOB.")),
                None => Rendered::exact(CLOB),
            },
            Platform::I | Platform::Zos => {
                let (fix, var) = if self.platform == Platform::I { ("NCHAR", "NVARCHAR") } else { ("GRAPHIC", "VARGRAPHIC") };
                let r = match len {
                    Some(_) if fixed && n <= lim.graphic => Rendered::exact(format!("{fix}({n})")),
                    Some(n) if n <= lim.vargraphic => Rendered::exact(format!("{var}({n})")),
                    Some(n) => lob(format!("{n} caracteres Unicode superan los {} de {var}: se usa un LOB.", lim.vargraphic)),
                    None => Rendered::exact(self.unicode_lob()),
                };
                if self.platform == Platform::Zos {
                    r.with(Info, TypeChanged, "Texto Unicode como GRAPHIC: queda en UTF-16 si la tabla se crea con CCSID UNICODE.")
                } else {
                    r
                }
            }
        }
    }

    fn binary(&self, len: Option<u32>, fixed: bool) -> Rendered {
        let lim = self.limits();
        match len {
            Some(n) if fixed && n <= lim.binary => Rendered::exact(format!("BINARY({n})")),
            Some(n) if n <= lim.varbinary => Rendered::exact(format!("VARBINARY({n})")),
            _ => Rendered::exact(BLOB),
        }
    }
}

/// `DB2GSE.ST_POINT`, `QSYS2.ST_POLYGON`, `ST_GEOMETRY` → the shape.
fn spatial_kind(name: &str) -> Option<Option<String>> {
    let base = name.rsplit('.').next().unwrap_or(name);
    let kind = base.strip_prefix("st_")?;
    Some((kind != "geometry").then(|| kind.to_string()))
}

impl Dialect for Db2 {
    fn id(&self) -> &'static str {
        "db2"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        // `VARCHAR(20 CODEUNITS32)`, `CLOB(1 M)`: the number comes first.
        let len = || {
            let a = t.args.first()?.trim().to_ascii_uppercase();
            let (num, unit) = a.split_at(a.find(|c: char| !c.is_ascii_digit()).unwrap_or(a.len()));
            let n: u32 = num.parse().ok()?;
            Some(match unit.trim() {
                "K" => n.saturating_mul(1024),
                "M" => n.saturating_mul(1024 * 1024),
                "G" => n.saturating_mul(1024 * 1024 * 1024),
                _ => n,
            })
        };
        // `CHAR(16) FOR BIT DATA`, `CHAR () FOR BIT DATA`, `LONG VARCHAR FOR BIT DATA`.
        let (name, bit_data) = match t.name.strip_suffix(" for bit data") {
            Some(n) => (n, true),
            None => (t.name.as_str(), t.has("for") && t.has("bit") && t.has("data")),
        };
        // No parentheses at all means the SQL default of 1; empty ones
        // (`CHAR ()`) mean the catalog didn't say.
        let fixed_len = || len().or((!t.raw.contains('(')).then_some(1));
        let unicode = self.platform == Platform::Luw || t.rest.iter().any(|r| matches!(r.as_str(), "1200" | "1208" | "unicode"));
        match name {
            "boolean" => L::Bool,
            "smallint" => L::int(2),
            "integer" | "int" => L::int(4),
            "bigint" => L::int(8),
            "decimal" | "dec" | "numeric" | "num" => L::Decimal { precision: p(0).or(Some(5)), scale: p(1).or(Some(0)) },
            // Decimal floating point: 16 or 34 significant digits.
            "decfloat" => L::Decimal { precision: None, scale: None },
            "real" => L::Float { bytes: 4 },
            "double" | "double precision" => L::Float { bytes: 8 },
            "float" => L::Float { bytes: if p(0).is_some_and(|b| b <= 24) { 4 } else { 8 } },
            "char" | "character" if bit_data => L::Binary { len: fixed_len() },
            "varchar" | "character varying" | "char varying" if bit_data => L::Varbinary { len: len() },
            "long varchar" if bit_data => L::Blob,
            "char" | "character" => L::Char { len: fixed_len(), unicode },
            "varchar" | "character varying" | "char varying" => L::Varchar { len: len(), unicode },
            "long varchar" => L::Text { unicode },
            "clob" | "character large object" | "char large object" => L::Text { unicode },
            "graphic" | "nchar" | "national character" | "national char" => L::Char { len: fixed_len(), unicode: true },
            "vargraphic" | "nvarchar" | "national character varying" | "national char varying" | "nchar varying" => {
                L::Varchar { len: len(), unicode: true }
            }
            "long vargraphic" | "dbclob" | "nclob" | "national character large object" | "nchar large object" => L::Text { unicode: true },
            "binary" => L::Binary { len: fixed_len() },
            "varbinary" | "binary varying" => L::Varbinary { len: len() },
            "blob" | "binary large object" => L::Blob,
            "date" => L::Date,
            // Db2 TIME has no fractional seconds.
            "time" => L::Time { precision: Some(0), tz: false },
            "timestamp" | "timestmp" => L::Timestamp { precision: Some(p(0).unwrap_or(6).min(TIMESTAMP_MAX as u32) as u8), tz: t.with_tz },
            "xml" => L::Xml,
            "rowid" => L::RowVersion,
            n => match spatial_kind(n) {
                Some(kind) => L::Geometry { kind, srid: None, geography: false },
                None => L::Other { native: t.raw.clone() },
            },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let lim = self.limits();
        let zos = self.platform == Platform::Zos;
        match t {
            L::Bool if zos => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Db2 for z/OS no tiene BOOLEAN: se usa SMALLINT (0/1)."),
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => {
                let r = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 | 2 => Rendered::exact("SMALLINT"),
                    3 | 4 => Rendered::exact("INTEGER"),
                    8 => Rendered::exact("BIGINT"),
                    _ if *bytes == 8 => Rendered::exact("DECIMAL(20, 0)"),
                    _ if lim.decimal >= 39 => Rendered::exact("DECIMAL(39, 0)"),
                    _ => Rendered::exact(format!("DECIMAL({}, 0)", lim.decimal))
                        .with(Loss, RangeLoss, format!("Entero de 16 bytes: {} admite hasta {} dígitos.", self.name(), lim.decimal)),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, "Db2 no tiene enteros sin signo: se usa un tipo más grande con signo.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } if *p <= lim.decimal => Rendered::exact(format!("DECIMAL({p}, {})", scale.unwrap_or(0))),
            // DECFLOAT(34) keeps 34 significant digits at any scale.
            L::Decimal { precision: Some(p), .. } if *p <= 34 => Rendered::exact("DECFLOAT(34)")
                .with(Info, TypeChanged, format!("DECIMAL de {p} dígitos supera los {} de {}: se usa DECFLOAT(34), que los guarda exactos.", lim.decimal, self.name())),
            L::Decimal { precision: Some(p), .. } => Rendered::exact("DECFLOAT(34)")
                .with(Loss, PrecisionLoss, format!("{} guarda hasta 34 dígitos significativos (DECFLOAT); el origen tiene {p}.", self.name())),
            L::Decimal { precision: None, .. } => Rendered::exact("DECFLOAT(34)")
                .with(Loss, PrecisionLoss, "Número sin precisión fija como DECFLOAT(34): hasta 34 dígitos significativos."),
            L::Float { bytes: 4 } => Rendered::exact("REAL"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(Info, TypeChanged, "Moneda como DECIMAL(19, 4)."),
            L::Char { len, unicode } => self.text(len.or(Some(1)), *unicode, true),
            L::Varchar { len, unicode } => self.text(*len, *unicode, false),
            L::Text { unicode } => {
                if *unicode {
                    Rendered::exact(self.unicode_lob())
                } else {
                    Rendered::exact(CLOB)
                }
            }
            L::Binary { len } => self.binary(len.or(Some(1)), true),
            L::Varbinary { len } => self.binary(*len, false),
            L::Blob => Rendered::exact(BLOB),
            L::Bit { len: Some(1) } => self.render_type(&L::Bool),
            L::Bit { len } => self.binary(len.map(|n| n.div_ceil(8)), false)
                .with(Warning, TypeApproximated, "Db2 no tiene cadenas de bits: se guardan como binario."),
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => {
                let mut r = Rendered::exact("TIME");
                if precision.is_some_and(|p| p > 0) {
                    r = r.with(Loss, PrecisionLoss, "El TIME de Db2 no guarda fracciones de segundo.");
                }
                if *tz {
                    r = r.with(Loss, TimeZoneLoss, "Db2 no guarda la zona horaria de una hora.");
                }
                r
            }
            L::Timestamp { precision, tz } => {
                let base = format!("TIMESTAMP{}", prec(*precision, TIMESTAMP_MAX));
                let r = match (tz, zos) {
                    (true, true) => Rendered::exact(format!("{base} WITH TIME ZONE")),
                    (true, false) => Rendered::exact(base)
                        .with(Loss, TimeZoneLoss, format!("{} no tiene TIMESTAMP WITH TIME ZONE: se guarda la fecha y hora sin la zona.", self.name())),
                    _ => Rendered::exact(base),
                };
                r.with_loss(precision_loss(*precision, TIMESTAMP_MAX))
            }
            L::Interval => Rendered::exact("VARCHAR(100)").with(Warning, TypeApproximated, "Db2 no tiene intervalos: se guardan como texto."),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            L::Uuid => Rendered::exact("CHAR(36)").with(Info, TypeChanged, "Db2 no tiene UUID: se guarda como texto de 36 caracteres."),
            L::Json { .. } => Rendered::exact(CLOB).with(Info, TypeChanged, "Db2 no tiene tipo JSON: se guarda como CLOB (JSON_VALUE y JSON_TABLE trabajan sobre él)."),
            L::Xml => Rendered::exact("XML"),
            L::Enum { values } | L::Set { values } => {
                let r = self.text(Some(longest(values) as u32), true, false);
                Rendered { native: r.native, notes: vec![] }
                    .with(Warning, TypeApproximated, format!("Db2 no tiene enumerados: queda como texto. Valores: {}.", values.join(", ")))
            }
            L::Array { .. } | L::Map { .. } => Rendered::exact(CLOB)
                .with(Warning, TypeApproximated, "Db2 no tiene arreglos ni mapas en columnas: se guardan como JSON en un CLOB."),
            L::Geometry { kind, .. } => {
                let schema = if self.platform == Platform::I { "QSYS2" } else { "DB2GSE" };
                let shape = match kind.as_deref() {
                    Some(k @ ("point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon")) => k.to_ascii_uppercase(),
                    _ => "GEOMETRY".into(),
                };
                Rendered::exact(format!("{schema}.ST_{shape}"))
                    .with(Warning, TypeChanged, "Los tipos espaciales requieren el soporte espacial de Db2 habilitado; el SRID va en cada valor.")
            }
            L::Inet => Rendered::exact("VARCHAR(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("CHAR(8) FOR BIT DATA")
                .with(Warning, TypeApproximated, "La versión de fila queda como binario y no se actualiza sola (en Db2 se usa ROW CHANGE TIMESTAMP)."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp => Some(match ty {
                L::Date => "CURRENT DATE".into(),
                L::Time { .. } => "CURRENT TIME".into(),
                _ => "CURRENT TIMESTAMP".into(),
            }),
            DefaultValue::CurrentDate => Some("CURRENT DATE".into()),
            DefaultValue::CurrentTime => Some("CURRENT TIME".into()),
            // GENERATE_UNIQUE() isn't allowed in a DEFAULT clause.
            DefaultValue::NewUuid => None,
            other => standard_default(other, ty, "CURRENT TIMESTAMP", None, self.platform == Platform::Zos),
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            on_delete: &["CASCADE", "SET NULL", "RESTRICT", "NO ACTION"],
            on_update: &["RESTRICT", "NO ACTION"],
            indexes: true,
            // Db2 for i has sparse indexes (CREATE INDEX … WHERE).
            partial_indexes: self.platform == Platform::I,
            supports_include: true,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: true,
            // z/OS column names: 30 bytes; the rest: 128.
            max_identifier: if self.platform == Platform::Zos { 30 } else { 128 },
            case: IdentCase::Upper,
        }
    }
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static LUW: Db2 = Db2 { platform: Platform::Luw };
    static I: Db2 = Db2 { platform: Platform::I };
    static ZOS: Db2 = Db2 { platform: Platform::Zos };
    Some(match driver_id {
        "db2" => &LUW,
        "db2i" => &I,
        "db2zos" => &ZOS,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn luw() -> &'static dyn Dialect {
        lookup("db2").unwrap()
    }
    fn zos() -> &'static dyn Dialect {
        lookup("db2zos").unwrap()
    }
    fn i() -> &'static dyn Dialect {
        lookup("db2i").unwrap()
    }
    fn p(d: &dyn Dialect, s: &str) -> L {
        d.parse_type(&parse(s))
    }
    fn r(d: &dyn Dialect, t: L) -> String {
        d.render_type(&t).native
    }

    #[test]
    fn parses_catalog_spellings() {
        let d = luw();
        assert_eq!(p(d, "SMALLINT"), L::int(2));
        assert_eq!(p(d, "INTEGER"), L::int(4));
        assert_eq!(p(d, "BIGINT"), L::int(8));
        assert_eq!(p(d, "DECIMAL(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(p(d, "DECIMAL"), L::Decimal { precision: Some(5), scale: Some(0) });
        assert_eq!(p(d, "NUMERIC(31,0)"), L::Decimal { precision: Some(31), scale: Some(0) });
        assert_eq!(p(d, "DECFLOAT"), L::Decimal { precision: None, scale: None });
        assert_eq!(p(d, "DECFLOAT(16)"), L::Decimal { precision: None, scale: None });
        assert_eq!(p(d, "REAL"), L::Float { bytes: 4 });
        assert_eq!(p(d, "DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(p(d, "FLOAT(21)"), L::Float { bytes: 4 });
        assert_eq!(p(d, "CHAR(10)"), L::Char { len: Some(10), unicode: true });
        assert_eq!(p(d, "CHARACTER"), L::Char { len: Some(1), unicode: true });
        assert_eq!(p(d, "VARCHAR(40)"), L::Varchar { len: Some(40), unicode: true });
        assert_eq!(p(d, "VARCHAR(20 CODEUNITS32)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(p(d, "LONG VARCHAR"), L::Text { unicode: true });
        assert_eq!(p(d, "CLOB"), L::Text { unicode: true });
        assert_eq!(p(d, "CLOB(1M)"), L::Text { unicode: true });
        assert_eq!(p(d, "GRAPHIC(5)"), L::Char { len: Some(5), unicode: true });
        assert_eq!(p(d, "VARGRAPHIC(100)"), L::Varchar { len: Some(100), unicode: true });
        assert_eq!(p(d, "NVARCHAR(100)"), L::Varchar { len: Some(100), unicode: true });
        assert_eq!(p(d, "LONG VARGRAPHIC"), L::Text { unicode: true });
        assert_eq!(p(d, "DBCLOB"), L::Text { unicode: true });
        assert_eq!(p(d, "NCLOB"), L::Text { unicode: true });
        assert_eq!(p(d, "BLOB"), L::Blob);
        assert_eq!(p(d, "BINARY(16)"), L::Binary { len: Some(16) });
        assert_eq!(p(d, "VARBINARY(200)"), L::Varbinary { len: Some(200) });
        assert_eq!(p(d, "CHAR () FOR BIT DATA"), L::Binary { len: None });
        assert_eq!(p(d, "CHAR(16) FOR BIT DATA"), L::Binary { len: Some(16) });
        assert_eq!(p(d, "VARCHAR () FOR BIT DATA"), L::Varbinary { len: None });
        assert_eq!(p(d, "VARCHAR(64) FOR BIT DATA"), L::Varbinary { len: Some(64) });
        assert_eq!(p(d, "LONG VARCHAR FOR BIT DATA"), L::Blob);
        assert_eq!(p(d, "DATE"), L::Date);
        assert_eq!(p(d, "TIME"), L::Time { precision: Some(0), tz: false });
        assert_eq!(p(d, "TIMESTAMP"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(p(d, "TIMESTAMP(12)"), L::Timestamp { precision: Some(12), tz: false });
        assert_eq!(p(d, "BOOLEAN"), L::Bool);
        assert_eq!(p(d, "XML"), L::Xml);
        assert_eq!(p(d, "DB2GSE.ST_POINT"), L::Geometry { kind: Some("point".into()), srid: None, geography: false });
        assert_eq!(p(d, "DB2GSE.ST_GEOMETRY"), L::Geometry { kind: None, srid: None, geography: false });
        assert!(matches!(p(d, "DATALINK"), L::Other { .. }));
    }

    #[test]
    fn platform_differences_in_parsing() {
        // EBCDIC by default on i and z/OS; CCSID 1208/1200 is Unicode.
        assert_eq!(p(i(), "VARCHAR(40)"), L::Varchar { len: Some(40), unicode: false });
        assert_eq!(p(i(), "VARCHAR(40) CCSID 1208"), L::Varchar { len: Some(40), unicode: true });
        assert_eq!(p(i(), "NCHAR(4)"), L::Char { len: Some(4), unicode: true });
        assert_eq!(p(zos(), "CHAR(3)"), L::Char { len: Some(3), unicode: false });
        assert_eq!(p(zos(), "TIMESTAMP(6) WITH TIME ZONE"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(p(zos(), "ROWID"), L::RowVersion);
        assert_eq!(p(i(), "QSYS2.ST_POLYGON"), L::Geometry { kind: Some("polygon".into()), srid: None, geography: false });
    }

    #[test]
    fn renders_every_variant() {
        let d = luw();
        assert_eq!(r(d, L::Bool), "BOOLEAN");
        assert_eq!(r(zos(), L::Bool), "SMALLINT");
        assert_eq!(r(d, L::int(1)), "SMALLINT");
        assert_eq!(r(d, L::int(4)), "INTEGER");
        assert_eq!(r(d, L::Int { bytes: 4, unsigned: true }), "BIGINT");
        assert_eq!(r(d, L::Int { bytes: 8, unsigned: true }), "DECIMAL(20, 0)");
        assert_eq!(r(d, L::int(16)), "DECIMAL(31, 0)");
        assert_eq!(r(i(), L::int(16)), "DECIMAL(39, 0)");
        assert_eq!(r(d, L::Decimal { precision: Some(12), scale: Some(2) }), "DECIMAL(12, 2)");
        assert_eq!(r(d, L::Decimal { precision: Some(33), scale: Some(2) }), "DECFLOAT(34)");
        assert_eq!(r(i(), L::Decimal { precision: Some(50), scale: Some(2) }), "DECIMAL(50, 2)");
        assert_eq!(r(d, L::Decimal { precision: None, scale: None }), "DECFLOAT(34)");
        assert_eq!(r(d, L::Float { bytes: 4 }), "REAL");
        assert_eq!(r(d, L::Float { bytes: 8 }), "DOUBLE");
        assert_eq!(r(d, L::Money), "DECIMAL(19, 4)");
        assert_eq!(r(d, L::Char { len: Some(3), unicode: false }), "CHAR(3)");
        assert_eq!(r(d, L::Char { len: Some(300), unicode: false }), "VARCHAR(300)");
        assert_eq!(r(d, L::Char { len: Some(3), unicode: true }), "CHAR(3 CODEUNITS32)");
        assert_eq!(r(d, L::Varchar { len: Some(40), unicode: true }), "VARCHAR(40 CODEUNITS32)");
        assert_eq!(r(d, L::Varchar { len: Some(9000), unicode: true }), "CLOB(1G)");
        assert_eq!(r(d, L::Varchar { len: Some(9000), unicode: false }), "VARCHAR(9000)");
        assert_eq!(r(d, L::Varchar { len: Some(40000), unicode: false }), "CLOB(1G)");
        assert_eq!(r(i(), L::Varchar { len: Some(40), unicode: true }), "NVARCHAR(40)");
        assert_eq!(r(i(), L::Char { len: Some(4), unicode: true }), "NCHAR(4)");
        assert_eq!(r(zos(), L::Varchar { len: Some(40), unicode: true }), "VARGRAPHIC(40)");
        assert_eq!(r(zos(), L::Text { unicode: true }), "DBCLOB(512M)");
        assert_eq!(r(d, L::Text { unicode: true }), "CLOB(1G)");
        assert_eq!(r(d, L::Binary { len: Some(16) }), "BINARY(16)");
        assert_eq!(r(d, L::Binary { len: Some(300) }), "VARBINARY(300)");
        assert_eq!(r(d, L::Varbinary { len: Some(300) }), "VARBINARY(300)");
        assert_eq!(r(d, L::Varbinary { len: None }), "BLOB(1G)");
        assert_eq!(r(d, L::Blob), "BLOB(1G)");
        assert_eq!(r(d, L::Bit { len: Some(1) }), "BOOLEAN");
        assert_eq!(r(d, L::Bit { len: Some(12) }), "VARBINARY(2)");
        assert_eq!(r(d, L::Date), "DATE");
        assert_eq!(r(d, L::Time { precision: Some(3), tz: true }), "TIME");
        assert_eq!(d.render_type(&L::Time { precision: Some(3), tz: true }).notes.len(), 2);
        assert_eq!(r(d, L::Timestamp { precision: None, tz: false }), "TIMESTAMP");
        assert_eq!(r(d, L::Timestamp { precision: Some(3), tz: false }), "TIMESTAMP(3)");
        assert_eq!(r(d, L::Timestamp { precision: Some(6), tz: true }), "TIMESTAMP(6)");
        assert!(d.render_type(&L::Timestamp { precision: Some(6), tz: true }).notes.iter().any(|n| n.code == IssueCode::TimeZoneLoss));
        assert_eq!(r(zos(), L::Timestamp { precision: Some(6), tz: true }), "TIMESTAMP(6) WITH TIME ZONE");
        assert_eq!(r(d, L::Interval), "VARCHAR(100)");
        assert_eq!(r(d, L::Year), "SMALLINT");
        assert_eq!(r(d, L::Uuid), "CHAR(36)");
        assert_eq!(r(d, L::Json { binary: true }), "CLOB(1G)");
        assert_eq!(r(d, L::Xml), "XML");
        assert_eq!(r(d, L::Enum { values: vec!["a".into(), "bcd".into()] }), "VARCHAR(3 CODEUNITS32)");
        assert_eq!(r(d, L::Set { values: vec!["a".into()] }), "VARCHAR(1 CODEUNITS32)");
        assert_eq!(r(d, L::Array { of: Box::new(L::int(4)) }), "CLOB(1G)");
        assert_eq!(r(d, L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::int(4)) }), "CLOB(1G)");
        assert_eq!(r(d, L::Geometry { kind: Some("point".into()), srid: Some(4326), geography: false }), "DB2GSE.ST_POINT");
        assert_eq!(r(i(), L::Geometry { kind: None, srid: None, geography: true }), "QSYS2.ST_GEOMETRY");
        assert_eq!(r(d, L::Inet), "VARCHAR(45)");
        assert_eq!(r(d, L::MacAddr), "VARCHAR(17)");
        assert_eq!(r(d, L::RowVersion), "CHAR(8) FOR BIT DATA");
        assert_eq!(r(d, L::Other { native: "DATALINK".into() }), "DATALINK");
    }

    #[test]
    fn round_trips_its_own_spellings() {
        for d in [luw(), i(), zos()] {
            for t in [
                L::int(2),
                L::int(4),
                L::int(8),
                L::Decimal { precision: Some(12), scale: Some(2) },
                L::Float { bytes: 8 },
                L::Varchar { len: Some(40), unicode: true },
                L::Char { len: Some(3), unicode: true },
                L::Blob,
                L::Date,
                L::Timestamp { precision: Some(3), tz: false },
                L::Xml,
            ] {
                let native = d.render_type(&t).native;
                assert_eq!(d.parse_type(&parse(&native)), t, "{native} on {}", d.id());
            }
        }
    }

    #[test]
    fn defaults() {
        let d = luw();
        let ts = L::Timestamp { precision: Some(6), tz: false };
        assert_eq!(d.render_default(&DefaultValue::CurrentTimestamp, &ts).as_deref(), Some("CURRENT TIMESTAMP"));
        assert_eq!(d.render_default(&DefaultValue::CurrentTimestamp, &L::Date).as_deref(), Some("CURRENT DATE"));
        assert_eq!(d.render_default(&DefaultValue::CurrentDate, &L::Date).as_deref(), Some("CURRENT DATE"));
        assert_eq!(d.render_default(&DefaultValue::CurrentTime, &L::Time { precision: None, tz: false }).as_deref(), Some("CURRENT TIME"));
        assert_eq!(d.render_default(&DefaultValue::Bool(true), &L::Bool).as_deref(), Some("TRUE"));
        assert_eq!(zos().render_default(&DefaultValue::Bool(true), &L::Bool).as_deref(), Some("1"));
        assert_eq!(d.render_default(&DefaultValue::NewUuid, &L::Uuid), None);
        assert_eq!(d.render_default(&DefaultValue::Text("it's".into()), &L::Varchar { len: Some(9), unicode: true }).as_deref(), Some("'it''s'"));
    }

    #[test]
    fn caps_per_platform() {
        assert!(!luw().caps().partial_indexes);
        assert!(i().caps().partial_indexes);
        assert_eq!(zos().caps().max_identifier, 30);
        assert_eq!(luw().caps().case, IdentCase::Upper);
        assert!(lookup("db2x").is_none());
    }
}
