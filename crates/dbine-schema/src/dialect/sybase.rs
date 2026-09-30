//! SAP's two Sybase engines, which share a name and not much else:
//!
//! - **SAP ASE** (Adaptive Server Enterprise, id `sybase`): T-SQL, types
//!   close to SQL Server's plus Unicode `unichar`/`univarchar`/`unitext`,
//!   unsigned integers, `bigdatetime`/`bigtime` with microseconds, and a
//!   `bit` that can't be NULL. Column sizes depend on the page size; the
//!   limits here are those of the default 2 KB pages.
//! - **SAP SQL Anywhere** (id `sqlanywhere`): Watcom SQL, `long varchar`,
//!   `uniqueidentifier`, `timestamp with time zone`, bit strings, spatial
//!   types, NUMERIC up to 127 digits.
//!
//! Both are read through ODBC, which reports lower-case names with the
//! length (`varchar(40)`, `numeric(12,2)`, `unsigned int`).

use super::postgres::{longest, precision_loss};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct Ase;
pub struct SqlAnywhere;

/// The type name; ODBC spells unsigned types `unsigned int`, which the
/// parser splits into the flag and a trailing word.
fn base_name(t: &TypeSpec) -> &str {
    if t.name.is_empty() {
        t.rest.first().map_or("", String::as_str)
    } else {
        t.name.as_str()
    }
}

fn geometry_kind(name: &str) -> Option<Option<String>> {
    let kind = name.strip_prefix("st_")?;
    Some((kind != "geometry").then(|| kind.to_string()))
}

// ------------------------------------------------------------------ ASE

/// Largest char/varchar/binary column on 2 KB pages, in bytes.
const ASE_MAX_BYTES: u32 = 1962;
/// unichar/univarchar: two bytes per character.
const ASE_MAX_UNICHARS: u32 = ASE_MAX_BYTES / 2;
const ASE_MAX_DECIMAL: u32 = 38;

impl Dialect for Ase {
    fn id(&self) -> &'static str {
        "sybase"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let u = t.unsigned;
        match base_name(t) {
            "bit" => L::Bool,
            "tinyint" => L::Int { bytes: 1, unsigned: true },
            "smallint" => L::Int { bytes: 2, unsigned: u },
            "int" | "integer" => L::Int { bytes: 4, unsigned: u },
            "bigint" => L::Int { bytes: 8, unsigned: u },
            // systypes names of the unsigned types.
            "usmallint" => L::Int { bytes: 2, unsigned: true },
            "uint" => L::Int { bytes: 4, unsigned: true },
            "ubigint" => L::Int { bytes: 8, unsigned: true },
            "numeric" | "decimal" | "dec" => L::Decimal { precision: p(0).or(Some(18)), scale: p(1).or(Some(0)) },
            "money" | "smallmoney" => L::Money,
            "real" => L::Float { bytes: 4 },
            "double precision" => L::Float { bytes: 8 },
            // float(p): p below 16 is a real.
            "float" => L::Float { bytes: if p(0).is_some_and(|b| b < 16) { 4 } else { 8 } },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: false },
            "varchar" | "character varying" | "char varying" => L::Varchar { len: p(0).or(Some(1)), unicode: false },
            "nchar" | "national character" | "national char" | "unichar" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "nvarchar" | "national character varying" | "nchar varying" | "univarchar" => L::Varchar { len: p(0).or(Some(1)), unicode: true },
            "sysname" => L::Varchar { len: Some(30), unicode: false },
            "longsysname" => L::Varchar { len: Some(255), unicode: false },
            "text" => L::Text { unicode: false },
            "unitext" => L::Text { unicode: true },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" => L::Varbinary { len: p(0).or(Some(1)) },
            "image" => L::Blob,
            "date" => L::Date,
            // 1/300 s, shown with milliseconds.
            "time" => L::Time { precision: Some(3), tz: false },
            "bigtime" => L::Time { precision: Some(6), tz: false },
            "datetime" => L::Timestamp { precision: Some(3), tz: false },
            "smalldatetime" => L::Timestamp { precision: Some(0), tz: false },
            "bigdatetime" => L::Timestamp { precision: Some(6), tz: false },
            // The automatic row version, as in old SQL Server.
            "timestamp" => L::RowVersion,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let too_long = |what: &str, n: u32| format!("{what}({n}) supera el máximo de ASE con páginas de 2 KB: se usa un tipo de texto largo.");
        match t {
            L::Bool => Rendered::exact("bit"),
            L::Int { bytes: 1, unsigned: true } => Rendered::exact("tinyint"),
            L::Int { bytes: 1, unsigned: false } | L::Int { bytes: 2, unsigned: false } => Rendered::exact("smallint"),
            L::Int { bytes: 2, unsigned: true } => Rendered::exact("unsigned smallint"),
            L::Int { bytes: 3 | 4, unsigned: false } => Rendered::exact("int"),
            L::Int { bytes: 3 | 4, unsigned: true } => Rendered::exact("unsigned int"),
            L::Int { bytes: 8, unsigned: false } => Rendered::exact("bigint"),
            L::Int { bytes: 8, unsigned: true } => Rendered::exact("unsigned bigint"),
            L::Int { .. } => Rendered::exact("numeric(38, 0)").with(Loss, RangeLoss, "Entero de 16 bytes como numeric(38, 0): no entran los valores de 39 dígitos."),
            L::Decimal { precision: Some(p), scale } if *p <= ASE_MAX_DECIMAL => Rendered::exact(format!("numeric({p}, {})", scale.unwrap_or(0))),
            L::Decimal { precision: Some(p), scale } => Rendered::exact(format!("numeric(38, {})", scale.unwrap_or(0).min(38)))
                .with(Loss, PrecisionLoss, format!("ASE admite hasta 38 dígitos; el origen tiene {p}.")),
            L::Decimal { precision: None, .. } => Rendered::exact("numeric(38, 10)")
                .with(Loss, PrecisionLoss, "El origen no fija la precisión: se usa numeric(38, 10)."),
            L::Float { bytes: 4 } => Rendered::exact("real"),
            L::Float { .. } => Rendered::exact("double precision"),
            L::Money => Rendered::exact("money"),
            L::Char { len, unicode: false } => match len.unwrap_or(1) {
                n if n <= ASE_MAX_BYTES => Rendered::exact(format!("char({n})")),
                n => Rendered::exact("text").with(Info, TypeChanged, too_long("char", n)),
            },
            L::Char { len, unicode: true } => match len.unwrap_or(1) {
                n if n <= ASE_MAX_UNICHARS => Rendered::exact(format!("unichar({n})")),
                n => Rendered::exact("unitext").with(Info, TypeChanged, too_long("unichar", n)),
            },
            L::Varchar { len: Some(n), unicode: false } if *n <= ASE_MAX_BYTES => Rendered::exact(format!("varchar({n})")),
            L::Varchar { len: Some(n), unicode: true } if *n <= ASE_MAX_UNICHARS => Rendered::exact(format!("univarchar({n})")),
            L::Varchar { len: Some(n), unicode } => Rendered::exact(if *unicode { "unitext" } else { "text" })
                .with(Info, TypeChanged, too_long("varchar", *n)),
            L::Varchar { len: None, unicode } | L::Text { unicode } => Rendered::exact(if *unicode { "unitext" } else { "text" }),
            L::Binary { len } => match len.unwrap_or(1) {
                n if n <= ASE_MAX_BYTES => Rendered::exact(format!("binary({n})")),
                _ => Rendered::exact("image"),
            },
            L::Varbinary { len: Some(n) } if *n <= ASE_MAX_BYTES => Rendered::exact(format!("varbinary({n})")),
            L::Varbinary { .. } | L::Blob => Rendered::exact("image"),
            L::Bit { len: Some(1) } => Rendered::exact("bit"),
            L::Bit { len } => self.render_type(&L::Varbinary { len: len.map(|n| n.div_ceil(8)) })
                .with(Warning, TypeApproximated, "ASE no tiene cadenas de bits: se guardan como binario."),
            L::Date => Rendered::exact("date"),
            L::Time { precision, tz } => {
                let r = match precision {
                    Some(p) if *p <= 3 => Rendered::exact("time"),
                    None => Rendered::exact("time"),
                    p => Rendered::exact("bigtime").with_loss(precision_loss(*p, 6)),
                };
                if *tz {
                    r.with(Loss, TimeZoneLoss, "ASE no guarda la zona horaria.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let r = match precision {
                    Some(0) => Rendered::exact("smalldatetime").with(Info, TypeChanged, "smalldatetime guarda hasta el minuto: si hay segundos, usá datetime."),
                    Some(p) if *p <= 3 => Rendered::exact("datetime"),
                    p => Rendered::exact("bigdatetime").with_loss(precision_loss(*p, 6)),
                };
                if *tz {
                    r.with(Loss, TimeZoneLoss, "ASE no guarda la zona horaria: conviene convertir los valores a UTC al copiarlos.")
                } else {
                    r
                }
            }
            L::Interval => Rendered::exact("varchar(100)").with(Warning, TypeApproximated, "ASE no tiene intervalos: se guardan como texto."),
            L::Year => Rendered::exact("smallint").with(Info, TypeChanged, "Año como smallint."),
            L::Uuid => Rendered::exact("char(36)").with(Info, TypeChanged, "ASE no tiene UUID: se guarda como texto (newid(1) lo genera con guiones)."),
            L::Json { .. } => Rendered::exact("unitext").with(Info, TypeChanged, "ASE no tiene tipo JSON: se guarda como unitext."),
            L::Xml => Rendered::exact("unitext").with(Info, TypeChanged, "XML como unitext (las funciones XML de ASE trabajan sobre texto)."),
            L::Enum { values } | L::Set { values } => Rendered::exact(format!("univarchar({})", longest(values)))
                .with(Warning, TypeApproximated, format!("ASE no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } => Rendered::exact("unitext")
                .with(Warning, TypeApproximated, "ASE no tiene arreglos ni mapas: se guardan como JSON en texto."),
            L::Geometry { .. } => Rendered::exact("image").with(Warning, TypeApproximated, "ASE no tiene tipos espaciales: se guardan como binario (WKB)."),
            L::Inet => Rendered::exact("varchar(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("varchar(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("timestamp"),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        let native = self.render_type(ty).native;
        if matches!(native.as_str(), "text" | "unitext" | "image") && !matches!(d, DefaultValue::Null) {
            return None;
        }
        let now = match native.as_str() {
            "date" => "current_date()",
            "time" => "current_time()",
            "bigtime" => "current_bigtime()",
            "bigdatetime" => "current_bigdatetime()",
            _ => "getdate()",
        };
        match d {
            DefaultValue::CurrentTimestamp => Some(now.into()),
            DefaultValue::CurrentDate => Some("current_date()".into()),
            DefaultValue::CurrentTime => Some(if native == "bigtime" { "current_bigtime()" } else { "current_time()" }.into()),
            DefaultValue::NewUuid => Some("newid(1)".into()),
            other => standard_default(other, ty, now, None, true),
        }
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: true,
            // Declarative references always restrict; there are no
            // referential actions.
            on_delete: &[],
            on_update: &[],
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: false,
            max_identifier: 255,
            case: IdentCase::Preserve,
        }
    }

    fn implies_auto_increment(&self, t: &TypeSpec) -> bool {
        t.has("identity")
    }

    /// An ASE `bit` can't be NULL: nullable booleans become tinyint.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        for c in t.columns.iter_mut().filter(|c| c.nullable && c.data_type == "bit") {
            c.data_type = "tinyint".into();
            report.push(
                Severity::Info,
                IssueCode::TypeChanged,
                &t.name,
                Some(&c.name),
                "En ASE una columna bit no admite nulos: se usa tinyint (0/1) para conservar los NULL.",
            );
        }
    }
}

// ---------------------------------------------------------- SQL Anywhere

const SQLA_MAX_BYTES: u32 = 32767;
/// nchar/nvarchar lengths are characters.
const SQLA_MAX_NCHARS: u32 = 8191;
const SQLA_MAX_DECIMAL: u32 = 127;

impl Dialect for SqlAnywhere {
    fn id(&self) -> &'static str {
        "sqlanywhere"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let u = t.unsigned;
        match base_name(t) {
            "bit" => L::Bool,
            "tinyint" => L::Int { bytes: 1, unsigned: true },
            "smallint" => L::Int { bytes: 2, unsigned: u },
            "int" | "integer" => L::Int { bytes: 4, unsigned: u },
            "bigint" => L::Int { bytes: 8, unsigned: u },
            // Default NUMERIC is (30, 6).
            "numeric" | "decimal" | "dec" => L::Decimal { precision: p(0).or(Some(30)), scale: p(1).or(p(0).map_or(Some(6), |_| Some(0))) },
            "money" | "smallmoney" => L::Money,
            "real" => L::Float { bytes: 4 },
            "double" | "double precision" => L::Float { bytes: 8 },
            // FLOAT without precision is single precision.
            "float" => L::Float { bytes: if p(0).is_some_and(|b| b > 24) { 8 } else { 4 } },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: false },
            "varchar" | "character varying" | "char varying" => L::Varchar { len: p(0).or(Some(1)), unicode: false },
            "long varchar" | "text" => L::Text { unicode: false },
            "nchar" | "national character" | "national char" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "nvarchar" | "national character varying" | "nchar varying" => L::Varchar { len: p(0).or(Some(1)), unicode: true },
            "long nvarchar" | "ntext" => L::Text { unicode: true },
            "sysname" => L::Varchar { len: Some(128), unicode: false },
            "uniqueidentifier" | "uniqueidentifierstr" => L::Uuid,
            "xml" => L::Xml,
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" | "binary varying" => L::Varbinary { len: p(0).or(Some(1)) },
            "long binary" | "image" => L::Blob,
            "varbit" | "bit varying" => L::Bit { len: p(0).or(Some(1)) },
            "long varbit" => L::Bit { len: None },
            "date" => L::Date,
            "time" => L::Time { precision: Some(6), tz: false },
            "timestamp" | "datetime" => L::Timestamp { precision: Some(6), tz: t.with_tz },
            "smalldatetime" => L::Timestamp { precision: Some(0), tz: false },
            "datetimeoffset" => L::Timestamp { precision: Some(6), tz: true },
            n => match geometry_kind(n) {
                Some(kind) => L::Geometry { kind, srid: None, geography: false },
                None => L::Other { native: t.raw.clone() },
            },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("bit"),
            L::Int { bytes: 1, unsigned: true } => Rendered::exact("tinyint"),
            L::Int { bytes: 1 | 2, unsigned: false } => Rendered::exact("smallint"),
            L::Int { bytes: 2, unsigned: true } => Rendered::exact("unsigned smallint"),
            L::Int { bytes: 3 | 4, unsigned: false } => Rendered::exact("integer"),
            L::Int { bytes: 3 | 4, unsigned: true } => Rendered::exact("unsigned int"),
            L::Int { bytes: 8, unsigned: false } => Rendered::exact("bigint"),
            L::Int { bytes: 8, unsigned: true } => Rendered::exact("unsigned bigint"),
            L::Int { .. } => Rendered::exact("numeric(39, 0)").with(Info, TypeChanged, "Entero de 16 bytes como numeric(39, 0)."),
            L::Decimal { precision: Some(p), scale } if *p <= SQLA_MAX_DECIMAL => Rendered::exact(format!("numeric({p}, {})", scale.unwrap_or(0))),
            L::Decimal { precision: Some(p), scale } => Rendered::exact(format!("numeric({SQLA_MAX_DECIMAL}, {})", scale.unwrap_or(0).min(SQLA_MAX_DECIMAL)))
                .with(Loss, PrecisionLoss, format!("SQL Anywhere admite hasta {SQLA_MAX_DECIMAL} dígitos; el origen tiene {p}.")),
            L::Decimal { precision: None, .. } => Rendered::exact("numeric(127, 30)")
                .with(Loss, PrecisionLoss, "El origen no fija la precisión: se usa numeric(127, 30)."),
            L::Float { bytes: 4 } => Rendered::exact("real"),
            L::Float { .. } => Rendered::exact("double"),
            L::Money => Rendered::exact("money"),
            L::Char { len, unicode: false } => match len.unwrap_or(1) {
                n if n <= SQLA_MAX_BYTES => Rendered::exact(format!("char({n})")),
                n => Rendered::exact("long varchar").with(Info, TypeChanged, format!("char({n}) supera el máximo de 32767: se usa long varchar.")),
            },
            L::Char { len, unicode: true } => match len.unwrap_or(1) {
                n if n <= SQLA_MAX_NCHARS => Rendered::exact(format!("nchar({n})")),
                n => Rendered::exact("long nvarchar").with(Info, TypeChanged, format!("nchar({n}) supera el máximo de {SQLA_MAX_NCHARS}: se usa long nvarchar.")),
            },
            L::Varchar { len: Some(n), unicode: false } if *n <= SQLA_MAX_BYTES => Rendered::exact(format!("varchar({n})")),
            L::Varchar { len: Some(n), unicode: true } if *n <= SQLA_MAX_NCHARS => Rendered::exact(format!("nvarchar({n})")),
            L::Varchar { len: Some(n), unicode } => Rendered::exact(if *unicode { "long nvarchar" } else { "long varchar" })
                .with(Info, TypeChanged, format!("varchar({n}) supera el máximo de SQL Anywhere: se usa texto largo.")),
            L::Varchar { len: None, unicode } | L::Text { unicode } => Rendered::exact(if *unicode { "long nvarchar" } else { "long varchar" }),
            L::Binary { len } => match len.unwrap_or(1) {
                n if n <= SQLA_MAX_BYTES => Rendered::exact(format!("binary({n})")),
                _ => Rendered::exact("long binary"),
            },
            L::Varbinary { len: Some(n) } if *n <= SQLA_MAX_BYTES => Rendered::exact(format!("varbinary({n})")),
            L::Varbinary { .. } | L::Blob => Rendered::exact("long binary"),
            L::Bit { len: Some(1) } => Rendered::exact("bit"),
            L::Bit { len: Some(n) } if *n <= SQLA_MAX_BYTES => Rendered::exact(format!("varbit({n})")),
            L::Bit { .. } => Rendered::exact("long varbit"),
            L::Date => Rendered::exact("date"),
            L::Time { precision, tz } => {
                let r = Rendered::exact("time").with_loss(precision_loss(*precision, 6));
                if *tz {
                    r.with(Loss, TimeZoneLoss, "SQL Anywhere no guarda la zona horaria de una hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision: Some(0), tz: false } => Rendered::exact("smalldatetime").with(Info, TypeChanged, "smalldatetime guarda hasta el minuto."),
            L::Timestamp { precision, tz } => {
                Rendered::exact(if *tz { "timestamp with time zone" } else { "timestamp" }).with_loss(precision_loss(*precision, 6))
            }
            L::Interval => Rendered::exact("varchar(100)").with(Warning, TypeApproximated, "SQL Anywhere no tiene intervalos: se guardan como texto."),
            L::Year => Rendered::exact("smallint").with(Info, TypeChanged, "Año como smallint."),
            L::Uuid => Rendered::exact("uniqueidentifier"),
            L::Json { .. } => Rendered::exact("long nvarchar").with(Info, TypeChanged, "SQL Anywhere no tiene tipo JSON: se guarda como long nvarchar."),
            L::Xml => Rendered::exact("xml"),
            L::Enum { values } | L::Set { values } => Rendered::exact(format!("nvarchar({})", longest(values)))
                .with(Warning, TypeApproximated, format!("SQL Anywhere no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } => Rendered::exact("long nvarchar")
                .with(Warning, TypeApproximated, "SQL Anywhere no tiene arreglos ni mapas en columnas: se guardan como JSON en texto."),
            L::Geometry { kind, srid, geography } => {
                let base = match kind.as_deref() {
                    Some(k @ ("point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon")) => {
                        format!("ST_{}{}", k[..1].to_ascii_uppercase(), &k[1..])
                    }
                    _ => "ST_Geometry".into(),
                };
                match srid.or(geography.then_some(4326)) {
                    Some(s) => Rendered::exact(format!("{base}(SRID={s})")),
                    None => Rendered::exact(base),
                }
            }
            L::Inet => Rendered::exact("varchar(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("varchar(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("binary(8)")
                .with(Warning, TypeApproximated, "La versión de fila queda como binario y no se actualiza sola (en SQL Anywhere se usa DEFAULT TIMESTAMP)."),
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
            DefaultValue::NewUuid => Some("NEWID()".into()),
            other => standard_default(other, ty, "CURRENT TIMESTAMP", Some("NEWID()"), true),
        }
    }

    fn caps(&self) -> Caps {
        // RESTRICT is the default; there's no NO ACTION keyword.
        const ACTIONS: &[&str] = &["CASCADE", "SET NULL", "SET DEFAULT", "RESTRICT"];
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
            max_identifier: 128,
            case: IdentCase::Preserve,
        }
    }
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static ASE: Ase = Ase;
    static SQLA: SqlAnywhere = SqlAnywhere;
    Some(match driver_id {
        "sybase" => &ASE,
        "sqlanywhere" => &SQLA,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use dbine_driver::ColumnDef;

    fn pa(s: &str) -> L {
        Ase.parse_type(&parse(s))
    }
    fn ra(t: L) -> String {
        Ase.render_type(&t).native
    }
    fn ps(s: &str) -> L {
        SqlAnywhere.parse_type(&parse(s))
    }
    fn rs(t: L) -> String {
        SqlAnywhere.render_type(&t).native
    }

    #[test]
    fn ase_parses_catalog_spellings() {
        assert_eq!(pa("bit"), L::Bool);
        assert_eq!(pa("tinyint"), L::Int { bytes: 1, unsigned: true });
        assert_eq!(pa("smallint"), L::int(2));
        assert_eq!(pa("int"), L::int(4));
        assert_eq!(pa("bigint"), L::int(8));
        assert_eq!(pa("unsigned int"), L::Int { bytes: 4, unsigned: true });
        assert_eq!(pa("unsigned bigint"), L::Int { bytes: 8, unsigned: true });
        assert_eq!(pa("usmallint"), L::Int { bytes: 2, unsigned: true });
        assert_eq!(pa("numeric(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(pa("money"), L::Money);
        assert_eq!(pa("smallmoney"), L::Money);
        assert_eq!(pa("float"), L::Float { bytes: 8 });
        assert_eq!(pa("float(10)"), L::Float { bytes: 4 });
        assert_eq!(pa("real"), L::Float { bytes: 4 });
        assert_eq!(pa("char(10)"), L::Char { len: Some(10), unicode: false });
        assert_eq!(pa("varchar(40)"), L::Varchar { len: Some(40), unicode: false });
        assert_eq!(pa("unichar(5)"), L::Char { len: Some(5), unicode: true });
        assert_eq!(pa("univarchar(50)"), L::Varchar { len: Some(50), unicode: true });
        assert_eq!(pa("nvarchar(50)"), L::Varchar { len: Some(50), unicode: true });
        assert_eq!(pa("sysname"), L::Varchar { len: Some(30), unicode: false });
        assert_eq!(pa("text"), L::Text { unicode: false });
        assert_eq!(pa("unitext"), L::Text { unicode: true });
        assert_eq!(pa("binary(16)"), L::Binary { len: Some(16) });
        assert_eq!(pa("varbinary(100)"), L::Varbinary { len: Some(100) });
        assert_eq!(pa("image"), L::Blob);
        assert_eq!(pa("date"), L::Date);
        assert_eq!(pa("time"), L::Time { precision: Some(3), tz: false });
        assert_eq!(pa("bigtime"), L::Time { precision: Some(6), tz: false });
        assert_eq!(pa("datetime"), L::Timestamp { precision: Some(3), tz: false });
        assert_eq!(pa("smalldatetime"), L::Timestamp { precision: Some(0), tz: false });
        assert_eq!(pa("bigdatetime"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(pa("timestamp"), L::RowVersion);
        assert!(Ase.implies_auto_increment(&parse("numeric(10,0) identity")));
        assert!(matches!(pa("my_udt"), L::Other { .. }));
    }

    #[test]
    fn ase_renders_every_variant() {
        assert_eq!(ra(L::Bool), "bit");
        assert_eq!(ra(L::Int { bytes: 1, unsigned: true }), "tinyint");
        assert_eq!(ra(L::int(1)), "smallint");
        assert_eq!(ra(L::Int { bytes: 2, unsigned: true }), "unsigned smallint");
        assert_eq!(ra(L::int(3)), "int");
        assert_eq!(ra(L::Int { bytes: 4, unsigned: true }), "unsigned int");
        assert_eq!(ra(L::Int { bytes: 8, unsigned: true }), "unsigned bigint");
        assert_eq!(ra(L::int(16)), "numeric(38, 0)");
        assert_eq!(ra(L::Decimal { precision: Some(12), scale: Some(2) }), "numeric(12, 2)");
        assert_eq!(ra(L::Decimal { precision: Some(50), scale: Some(2) }), "numeric(38, 2)");
        assert_eq!(ra(L::Decimal { precision: None, scale: None }), "numeric(38, 10)");
        assert_eq!(ra(L::Float { bytes: 4 }), "real");
        assert_eq!(ra(L::Float { bytes: 8 }), "double precision");
        assert_eq!(ra(L::Money), "money");
        assert_eq!(ra(L::Char { len: Some(3), unicode: false }), "char(3)");
        assert_eq!(ra(L::Char { len: Some(3), unicode: true }), "unichar(3)");
        assert_eq!(ra(L::Varchar { len: Some(40), unicode: false }), "varchar(40)");
        assert_eq!(ra(L::Varchar { len: Some(40), unicode: true }), "univarchar(40)");
        assert_eq!(ra(L::Varchar { len: Some(1000), unicode: true }), "unitext");
        assert_eq!(ra(L::Varchar { len: Some(3000), unicode: false }), "text");
        assert_eq!(ra(L::Text { unicode: true }), "unitext");
        assert_eq!(ra(L::Binary { len: Some(16) }), "binary(16)");
        assert_eq!(ra(L::Varbinary { len: Some(100) }), "varbinary(100)");
        assert_eq!(ra(L::Blob), "image");
        assert_eq!(ra(L::Bit { len: Some(1) }), "bit");
        assert_eq!(ra(L::Bit { len: Some(16) }), "varbinary(2)");
        assert_eq!(ra(L::Date), "date");
        assert_eq!(ra(L::Time { precision: Some(0), tz: false }), "time");
        assert_eq!(ra(L::Time { precision: Some(6), tz: false }), "bigtime");
        assert_eq!(ra(L::Timestamp { precision: Some(0), tz: false }), "smalldatetime");
        assert_eq!(ra(L::Timestamp { precision: Some(3), tz: false }), "datetime");
        assert_eq!(ra(L::Timestamp { precision: None, tz: true }), "bigdatetime");
        assert_eq!(ra(L::Timestamp { precision: Some(7), tz: false }), "bigdatetime");
        assert_eq!(ra(L::Interval), "varchar(100)");
        assert_eq!(ra(L::Year), "smallint");
        assert_eq!(ra(L::Uuid), "char(36)");
        assert_eq!(ra(L::Json { binary: true }), "unitext");
        assert_eq!(ra(L::Xml), "unitext");
        assert_eq!(ra(L::Enum { values: vec!["abc".into()] }), "univarchar(3)");
        assert_eq!(ra(L::Set { values: vec!["abc".into()] }), "univarchar(3)");
        assert_eq!(ra(L::Array { of: Box::new(L::int(4)) }), "unitext");
        assert_eq!(ra(L::Map { key: Box::new(L::int(4)), value: Box::new(L::int(4)) }), "unitext");
        assert_eq!(ra(L::Geometry { kind: None, srid: None, geography: false }), "image");
        assert_eq!(ra(L::Inet), "varchar(45)");
        assert_eq!(ra(L::MacAddr), "varchar(17)");
        assert_eq!(ra(L::RowVersion), "timestamp");
        assert_eq!(ra(L::Other { native: "my_udt".into() }), "my_udt");
    }

    #[test]
    fn ase_defaults_and_nullable_bits() {
        let d = |v: DefaultValue, t: L| Ase.render_default(&v, &t);
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: Some(3), tz: false }).as_deref(), Some("getdate()"));
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: Some(6), tz: false }).as_deref(), Some("current_bigdatetime()"));
        assert_eq!(d(DefaultValue::CurrentDate, L::Date).as_deref(), Some("current_date()"));
        assert_eq!(d(DefaultValue::CurrentTime, L::Time { precision: Some(6), tz: false }).as_deref(), Some("current_bigtime()"));
        assert_eq!(d(DefaultValue::Bool(true), L::Bool).as_deref(), Some("1"));
        assert_eq!(d(DefaultValue::NewUuid, L::Uuid).as_deref(), Some("newid(1)"));
        assert_eq!(d(DefaultValue::Text("x".into()), L::Text { unicode: true }), None);

        let mut t = TableSchema {
            name: "t".into(),
            columns: vec![
                ColumnDef { name: "a".into(), data_type: "bit".into(), nullable: true, ..Default::default() },
                ColumnDef { name: "b".into(), data_type: "bit".into(), nullable: false, ..Default::default() },
            ],
            ..Default::default()
        };
        let mut report = Report::default();
        Ase.finalize(&mut t, &mut report);
        assert_eq!(t.columns[0].data_type, "tinyint");
        assert_eq!(t.columns[1].data_type, "bit");
        assert_eq!(report.issues.len(), 1);
    }

    #[test]
    fn sqla_parses_catalog_spellings() {
        assert_eq!(ps("bit"), L::Bool);
        assert_eq!(ps("tinyint"), L::Int { bytes: 1, unsigned: true });
        assert_eq!(ps("integer"), L::int(4));
        assert_eq!(ps("unsigned int"), L::Int { bytes: 4, unsigned: true });
        assert_eq!(ps("unsigned smallint"), L::Int { bytes: 2, unsigned: true });
        assert_eq!(ps("bigint"), L::int(8));
        assert_eq!(ps("numeric(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(ps("numeric"), L::Decimal { precision: Some(30), scale: Some(6) });
        assert_eq!(ps("money"), L::Money);
        assert_eq!(ps("float"), L::Float { bytes: 4 });
        assert_eq!(ps("double"), L::Float { bytes: 8 });
        assert_eq!(ps("char(10)"), L::Char { len: Some(10), unicode: false });
        assert_eq!(ps("varchar(40)"), L::Varchar { len: Some(40), unicode: false });
        assert_eq!(ps("long varchar"), L::Text { unicode: false });
        assert_eq!(ps("nchar(3)"), L::Char { len: Some(3), unicode: true });
        assert_eq!(ps("nvarchar(40)"), L::Varchar { len: Some(40), unicode: true });
        assert_eq!(ps("long nvarchar"), L::Text { unicode: true });
        assert_eq!(ps("uniqueidentifier"), L::Uuid);
        assert_eq!(ps("uniqueidentifierstr"), L::Uuid);
        assert_eq!(ps("xml"), L::Xml);
        assert_eq!(ps("binary(16)"), L::Binary { len: Some(16) });
        assert_eq!(ps("varbinary(100)"), L::Varbinary { len: Some(100) });
        assert_eq!(ps("long binary"), L::Blob);
        assert_eq!(ps("varbit(12)"), L::Bit { len: Some(12) });
        assert_eq!(ps("long varbit"), L::Bit { len: None });
        assert_eq!(ps("date"), L::Date);
        assert_eq!(ps("time"), L::Time { precision: Some(6), tz: false });
        assert_eq!(ps("timestamp"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(ps("timestamp with time zone"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(ps("datetimeoffset"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(ps("smalldatetime"), L::Timestamp { precision: Some(0), tz: false });
        assert_eq!(ps("st_point"), L::Geometry { kind: Some("point".into()), srid: None, geography: false });
        assert!(matches!(ps("my_domain"), L::Other { .. }));
    }

    #[test]
    fn sqla_renders_every_variant() {
        assert_eq!(rs(L::Bool), "bit");
        assert_eq!(rs(L::Int { bytes: 1, unsigned: true }), "tinyint");
        assert_eq!(rs(L::int(1)), "smallint");
        assert_eq!(rs(L::Int { bytes: 2, unsigned: true }), "unsigned smallint");
        assert_eq!(rs(L::int(4)), "integer");
        assert_eq!(rs(L::Int { bytes: 8, unsigned: true }), "unsigned bigint");
        assert_eq!(rs(L::int(16)), "numeric(39, 0)");
        assert_eq!(rs(L::Decimal { precision: Some(60), scale: Some(2) }), "numeric(60, 2)");
        assert_eq!(rs(L::Decimal { precision: None, scale: None }), "numeric(127, 30)");
        assert_eq!(rs(L::Float { bytes: 4 }), "real");
        assert_eq!(rs(L::Float { bytes: 8 }), "double");
        assert_eq!(rs(L::Money), "money");
        assert_eq!(rs(L::Char { len: Some(3), unicode: false }), "char(3)");
        assert_eq!(rs(L::Char { len: Some(3), unicode: true }), "nchar(3)");
        assert_eq!(rs(L::Varchar { len: Some(40), unicode: true }), "nvarchar(40)");
        assert_eq!(rs(L::Varchar { len: Some(9000), unicode: true }), "long nvarchar");
        assert_eq!(rs(L::Text { unicode: false }), "long varchar");
        assert_eq!(rs(L::Binary { len: Some(16) }), "binary(16)");
        assert_eq!(rs(L::Varbinary { len: Some(100) }), "varbinary(100)");
        assert_eq!(rs(L::Blob), "long binary");
        assert_eq!(rs(L::Bit { len: Some(1) }), "bit");
        assert_eq!(rs(L::Bit { len: Some(12) }), "varbit(12)");
        assert_eq!(rs(L::Bit { len: None }), "long varbit");
        assert_eq!(rs(L::Date), "date");
        assert_eq!(rs(L::Time { precision: Some(7), tz: true }), "time");
        assert_eq!(SqlAnywhere.render_type(&L::Time { precision: Some(7), tz: true }).notes.len(), 2);
        assert_eq!(rs(L::Timestamp { precision: Some(3), tz: false }), "timestamp");
        assert_eq!(rs(L::Timestamp { precision: Some(0), tz: false }), "smalldatetime");
        assert_eq!(rs(L::Timestamp { precision: None, tz: true }), "timestamp with time zone");
        assert_eq!(rs(L::Interval), "varchar(100)");
        assert_eq!(rs(L::Year), "smallint");
        assert_eq!(rs(L::Uuid), "uniqueidentifier");
        assert_eq!(rs(L::Json { binary: true }), "long nvarchar");
        assert_eq!(rs(L::Xml), "xml");
        assert_eq!(rs(L::Enum { values: vec!["abc".into()] }), "nvarchar(3)");
        assert_eq!(rs(L::Set { values: vec!["abc".into()] }), "nvarchar(3)");
        assert_eq!(rs(L::Array { of: Box::new(L::int(4)) }), "long nvarchar");
        assert_eq!(rs(L::Map { key: Box::new(L::int(4)), value: Box::new(L::int(4)) }), "long nvarchar");
        assert_eq!(rs(L::Geometry { kind: Some("point".into()), srid: Some(4326), geography: false }), "ST_Point(SRID=4326)");
        assert_eq!(rs(L::Geometry { kind: None, srid: None, geography: false }), "ST_Geometry");
        assert_eq!(rs(L::Inet), "varchar(45)");
        assert_eq!(rs(L::MacAddr), "varchar(17)");
        assert_eq!(rs(L::RowVersion), "binary(8)");
        assert_eq!(rs(L::Other { native: "my_domain".into() }), "my_domain");
    }

    #[test]
    fn sqla_defaults() {
        let d = |v: DefaultValue, t: L| SqlAnywhere.render_default(&v, &t);
        assert_eq!(d(DefaultValue::CurrentTimestamp, L::Timestamp { precision: None, tz: false }).as_deref(), Some("CURRENT TIMESTAMP"));
        assert_eq!(d(DefaultValue::CurrentDate, L::Date).as_deref(), Some("CURRENT DATE"));
        assert_eq!(d(DefaultValue::NewUuid, L::Uuid).as_deref(), Some("NEWID()"));
        assert_eq!(d(DefaultValue::Bool(false), L::Bool).as_deref(), Some("0"));
    }

    #[test]
    fn round_trips() {
        for t in [
            L::Bool,
            L::Int { bytes: 4, unsigned: true },
            L::int(8),
            L::Decimal { precision: Some(12), scale: Some(2) },
            L::Varchar { len: Some(40), unicode: true },
            L::Char { len: Some(4), unicode: false },
            L::Text { unicode: true },
            L::Blob,
            L::Date,
            L::Timestamp { precision: Some(6), tz: false },
        ] {
            assert_eq!(pa(&ra(t.clone())), t, "ASE {}", ra(t.clone()));
            assert_eq!(ps(&rs(t.clone())), t, "SQLA {}", rs(t.clone()));
        }
        assert_eq!(ps(&rs(L::Uuid)), L::Uuid);
        assert_eq!(ps(&rs(L::Timestamp { precision: Some(6), tz: true })), L::Timestamp { precision: Some(6), tz: true });
    }
}
