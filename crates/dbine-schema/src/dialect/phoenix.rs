//! Apache Phoenix (SQL over HBase) and generic Avatica servers.
//!
//! Phoenix: the primary key is mandatory (it is the HBase row key), NOT
//! NULL only holds on key columns, there are no foreign keys, unique
//! indexes or comments, and `UNSIGNED_*` types are the non-negative halves
//! of the signed ones (sortable bytes), not wider ranges. DATE and TIME keep
//! a full date and time to the millisecond.
//!
//! Avatica is a protocol, not an engine: its server (Calcite, Druid…) owns
//! the types, which come in JDBC names. It reads as a source; as a target
//! the driver has no DDL, so every table is reported as left out.

use super::influxdb::source_only;
use super::postgres::longest;
use super::starrocks::{capped_decimal, wrap};
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::{quote, DefaultValue};
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{ColumnDef, KeyDef, TableSchema};

const AVATICA_WHY: &str = "Avatica no define DDL propio: las tablas se crean con el motor que está detrás del servidor.";

pub struct Phoenix {
    /// Generic Avatica: JDBC type names, no DDL.
    generic: bool,
}

/// Longest VARCHAR / DECIMAL precision Phoenix declares.
const MAX_PRECISION: u32 = 38;

/// Key column added to a table without a primary key.
pub const ROW_ID: &str = "ROW_ID";

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static PHOENIX: Phoenix = Phoenix { generic: false };
    static AVATICA: Phoenix = Phoenix { generic: true };
    match driver_id {
        "phoenix" => Some(&PHOENIX),
        "avatica" => Some(&AVATICA),
        _ => None,
    }
}

impl Dialect for Phoenix {
    fn id(&self) -> &'static str {
        if self.generic {
            "avatica"
        } else {
            "phoenix"
        }
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        // Phoenix's DATE and TIME carry a date and a time to the millisecond.
        let millis = L::Timestamp { precision: Some(3), tz: false };
        match t.name.as_str() {
            "boolean" | "bool" | "bit" => L::Bool,
            "tinyint" | "unsigned_tinyint" => L::int(1),
            "smallint" | "unsigned_smallint" => L::int(2),
            "integer" | "int" | "unsigned_int" => L::int(4),
            "bigint" | "unsigned_long" => L::int(8),
            "decimal" | "numeric" => L::Decimal { precision: p(0), scale: p(1).or(p(0).map(|_| 0)) },
            "float" | "unsigned_float" | "real" => L::Float { bytes: 4 },
            "double" | "unsigned_double" | "double precision" => L::Float { bytes: 8 },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: self.generic },
            "nchar" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "character varying" | "nvarchar" | "string" => match p(0) {
                Some(n) => L::Varchar { len: Some(n), unicode: true },
                None => L::Text { unicode: true },
            },
            "clob" | "nclob" | "text" => L::Text { unicode: true },
            "binary" => L::Binary { len: p(0) },
            "varbinary" | "binary varying" => match p(0) {
                Some(n) => L::Varbinary { len: Some(n) },
                None => L::Blob,
            },
            "blob" => L::Blob,
            "date" if self.generic => L::Date,
            "time" if self.generic => L::Time { precision: p(0).map(|x| x as u8), tz: t.with_tz },
            "timestamp" if self.generic => L::Timestamp { precision: p(0).map(|x| x as u8), tz: t.with_tz },
            "date" | "unsigned_date" => millis,
            "time" | "unsigned_time" => L::Time { precision: Some(3), tz: false },
            "timestamp" | "unsigned_timestamp" => L::Timestamp { precision: Some(9), tz: false },
            "timestamp_with_timezone" | "timestamp with time zone" => L::Timestamp { precision: p(0).map(|x| x as u8), tz: true },
            "interval" => L::Interval,
            "json" | "bson" => L::Json { binary: t.name == "bson" },
            "uuid" => L::Uuid,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let varchar = |why: String| Rendered::exact("VARCHAR").with(Warning, TypeApproximated, why);
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => {
                let b = L::signed_bytes_for(*bytes, *unsigned);
                let r = match b {
                    1 => Rendered::exact("TINYINT"),
                    2 => Rendered::exact("SMALLINT"),
                    3 | 4 => Rendered::exact("INTEGER"),
                    8 => Rendered::exact("BIGINT"),
                    _ => Rendered::exact("DECIMAL(38, 0)")
                        .with(Loss, RangeLoss, "Entero de 16 bytes como DECIMAL(38, 0): los valores de 39 dígitos no entran."),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, "Los UNSIGNED_* de Phoenix no amplían el rango: se usa un tipo más grande con signo.")
                } else {
                    r
                }
            }
            L::Decimal { precision: Some(p), scale } => capped_decimal(*p, *scale, MAX_PRECISION, "Phoenix"),
            // Phoenix's bare DECIMAL: maximum precision, variable scale.
            L::Decimal { precision: None, .. } => Rendered::exact("DECIMAL"),
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(Info, TypeChanged, "Moneda como DECIMAL(19, 4)."),
            // CHAR holds single-byte characters only.
            L::Char { len, unicode: false } => Rendered::exact(format!("CHAR({})", len.unwrap_or(1))),
            L::Char { len, .. } => Rendered::exact(format!("VARCHAR({})", len.unwrap_or(1)))
                .with(Info, TypeChanged, "CHAR de Phoenix solo admite caracteres de un byte: se usa VARCHAR (sin relleno)."),
            L::Varchar { len: Some(n), .. } => Rendered::exact(format!("VARCHAR({n})")),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact("VARCHAR"),
            L::Binary { len: Some(n) } => Rendered::exact(format!("BINARY({n})")),
            L::Binary { len: None } | L::Varbinary { .. } | L::Blob => Rendered::exact("VARBINARY"),
            L::Bit { len } => match len {
                Some(n) if *n <= 63 => Rendered::exact("BIGINT").with(Info, TypeChanged, "Cadena de bits como entero."),
                _ => Rendered::exact("VARBINARY").with(Warning, TypeApproximated, "Cadena de bits larga: se guarda como binario."),
            },
            L::Date => Rendered::exact("DATE").with(Info, TypeChanged, "DATE de Phoenix guarda también la hora (queda en 00:00)."),
            L::Time { precision, tz } => {
                let mut r = Rendered::exact("TIME");
                if precision.is_some_and(|p| p > 3) {
                    r = r.with(Loss, PrecisionLoss, "TIME de Phoenix guarda milisegundos.");
                }
                if *tz {
                    r = r.with(Loss, TimeZoneLoss, "Phoenix no guarda la zona horaria de una hora.");
                }
                r
            }
            L::Timestamp { tz, .. } => {
                let r = Rendered::exact("TIMESTAMP");
                if *tz {
                    r.with(Info, TimeZoneLoss, "Phoenix guarda el instante (en UTC), no la zona de origen.")
                } else {
                    r
                }
            }
            L::Interval => varchar("Phoenix no tiene intervalos: queda como texto.".into()),
            L::Year => Rendered::exact("SMALLINT").with(Info, TypeChanged, "Año como SMALLINT."),
            L::Uuid => Rendered::exact("CHAR(36)").with(Info, TypeChanged, "UUID como CHAR(36)."),
            L::Json { .. } => Rendered::exact("VARCHAR").with(Info, TypeApproximated, "JSON como texto."),
            L::Xml => varchar("XML como texto.".into()),
            L::Enum { values } => Rendered::exact(format!("VARCHAR({})", longest(values)))
                .with(Warning, TypeApproximated, format!("Phoenix no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => Rendered::exact("VARCHAR ARRAY").with(Warning, TypeApproximated, format!("Conjunto como arreglo de texto. Valores: {}.", values.join(", "))),
            L::Array { of } if !matches!(of.as_ref(), L::Array { .. } | L::Map { .. } | L::Json { .. }) => {
                wrap(self.render_type(of), |i| format!("{i} ARRAY"))
            }
            L::Array { .. } => varchar("Phoenix no tiene arreglos anidados: se guarda como texto JSON.".into()),
            L::Map { .. } => varchar("Phoenix no tiene mapas: se guarda como texto JSON.".into()),
            L::Geometry { .. } => varchar("Dato espacial como texto (WKT).".into()),
            L::Inet => Rendered::exact("VARCHAR(45)").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR(17)").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("BIGINT").with(Warning, TypeApproximated, "Phoenix no tiene versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::Null => "NULL".into(),
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => quote(s),
            DefaultValue::Bool(b) if matches!(ty, L::Bool) => if *b { "TRUE" } else { "FALSE" }.into(),
            DefaultValue::Bool(b) => if *b { "1" } else { "0" }.into(),
            // Only constant defaults: Phoenix refuses "stateful" ones
            // (CURRENT_DATE(), NEXT VALUE FOR…) in a column definition.
            _ => return None,
        })
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: false,
            on_delete: &[],
            on_update: &[],
            indexes: !self.generic,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: !self.generic,
            nullability: true,
            comments: false,
            max_identifier: 128,
            case: IdentCase::Upper,
        }
    }

    fn target_refusal(&self, _: &str) -> Option<&'static str> {
        self.generic.then_some(AVATICA_WHY)
    }

    /// The row key: a table without a primary key gets `ROW_ID`; NOT NULL
    /// stays only on key columns; unique indexes become plain ones.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        use IssueCode::*;
        use Severity::*;
        if self.generic {
            source_only(t, report, AVATICA_WHY);
            return;
        }
        let table = t.name.clone();
        let pk: Vec<String> = match &t.primary_key {
            Some(k) if !k.columns.is_empty() => k.columns.clone(),
            _ => {
                let name = if t.columns.iter().any(|c| c.name.eq_ignore_ascii_case(ROW_ID)) { format!("{ROW_ID}_1") } else { ROW_ID.to_string() };
                t.columns.insert(0, ColumnDef { name: name.clone(), data_type: "BIGINT".into(), nullable: false, ..Default::default() });
                t.primary_key = Some(KeyDef { name: None, columns: vec![name.clone()] });
                report.push(Warning, PrimaryKeyAdded, &table, Some(&name), format!(
                    "Phoenix exige clave primaria (es la clave de fila de HBase): se agrega «{name}». Al copiar hay que darle un valor único por fila (p. ej. NEXT VALUE FOR una secuencia)."
                ));
                vec![name]
            }
        };
        let immutable = t.options.get("IMMUTABLE_ROWS").is_some_and(|v| v.eq_ignore_ascii_case("true"));
        if !immutable {
            for c in t.columns.iter_mut().filter(|c| !c.nullable && !pk.contains(&c.name)) {
                c.nullable = true;
                report.push(Warning, NullabilityChanged, &table, Some(&c.name), "Phoenix solo admite NOT NULL en la clave primaria (o con IMMUTABLE_ROWS): la columna acepta nulos.");
            }
        }
        for ix in t.indexes.iter_mut().filter(|i| i.unique) {
            ix.unique = false;
            report.push(Warning, IndexChanged, &table, Some(&ix.name), "Phoenix no tiene índices únicos: queda como índice común y no garantiza unicidad.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn ph() -> &'static dyn Dialect {
        lookup("phoenix").unwrap()
    }
    fn p(d: &dyn Dialect, s: &str) -> L {
        crate::convert::logical_of(d, &parse(s))
    }

    #[test]
    fn parses_catalog_spellings() {
        let d = ph();
        assert_eq!(p(d, "INTEGER"), L::int(4));
        assert_eq!(p(d, "UNSIGNED_INT"), L::int(4));
        assert_eq!(p(d, "UNSIGNED_LONG"), L::int(8));
        assert_eq!(p(d, "TINYINT"), L::int(1));
        assert_eq!(p(d, "DECIMAL(10, 2)"), L::Decimal { precision: Some(10), scale: Some(2) });
        assert_eq!(p(d, "DECIMAL"), L::Decimal { precision: None, scale: None });
        assert_eq!(p(d, "FLOAT"), L::Float { bytes: 4 });
        assert_eq!(p(d, "UNSIGNED_DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(p(d, "BOOLEAN"), L::Bool);
        assert_eq!(p(d, "VARCHAR(20)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(p(d, "VARCHAR"), L::Text { unicode: true });
        assert_eq!(p(d, "CHAR(3)"), L::Char { len: Some(3), unicode: false });
        assert_eq!(p(d, "DATE"), L::Timestamp { precision: Some(3), tz: false });
        assert_eq!(p(d, "TIME"), L::Time { precision: Some(3), tz: false });
        assert_eq!(p(d, "TIMESTAMP"), L::Timestamp { precision: Some(9), tz: false });
        assert_eq!(p(d, "BINARY(16)"), L::Binary { len: Some(16) });
        assert_eq!(p(d, "VARBINARY"), L::Blob);
        assert_eq!(p(d, "VARCHAR ARRAY"), L::Array { of: Box::new(L::Text { unicode: true }) });
        assert_eq!(p(d, "INTEGER ARRAY"), L::Array { of: Box::new(L::int(4)) });
        let av = lookup("avatica").unwrap();
        assert_eq!(p(av, "DATE"), L::Date);
        assert_eq!(p(av, "TIMESTAMP(3)"), L::Timestamp { precision: Some(3), tz: false });
        assert_eq!(p(av, "CHAR(2)"), L::Char { len: Some(2), unicode: true });
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| ph().render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(3)), "INTEGER");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "BIGINT");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "DECIMAL(38, 0)");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "DECIMAL(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(65), scale: Some(2) }), "DECIMAL(38, 2)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "DECIMAL");
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Float { bytes: 8 }), "DOUBLE");
        assert_eq!(r(L::Money), "DECIMAL(19, 4)");
        assert_eq!(r(L::Char { len: Some(3), unicode: false }), "CHAR(3)");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }), "VARCHAR(3)");
        assert_eq!(r(L::Varchar { len: Some(20), unicode: true }), "VARCHAR(20)");
        assert_eq!(r(L::Text { unicode: true }), "VARCHAR");
        assert_eq!(r(L::Binary { len: Some(16) }), "BINARY(16)");
        assert_eq!(r(L::Varbinary { len: Some(16) }), "VARBINARY");
        assert_eq!(r(L::Blob), "VARBINARY");
        assert_eq!(r(L::Bit { len: Some(1) }), "BIGINT");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: Some(6), tz: false }), "TIME");
        assert_eq!(r(L::Timestamp { precision: Some(6), tz: true }), "TIMESTAMP");
        assert_eq!(r(L::Interval), "VARCHAR");
        assert_eq!(r(L::Year), "SMALLINT");
        assert_eq!(r(L::Uuid), "CHAR(36)");
        assert_eq!(r(L::Json { binary: true }), "VARCHAR");
        assert_eq!(r(L::Xml), "VARCHAR");
        assert_eq!(r(L::Enum { values: vec!["abc".into()] }), "VARCHAR(3)");
        assert_eq!(r(L::Set { values: vec![] }), "VARCHAR ARRAY");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }), "INTEGER ARRAY");
        assert_eq!(r(L::Array { of: Box::new(L::Array { of: Box::new(L::int(4)) }) }), "VARCHAR");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "VARCHAR");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "VARCHAR");
        assert_eq!(r(L::Inet), "VARCHAR(45)");
        assert_eq!(r(L::MacAddr), "VARCHAR(17)");
        assert_eq!(r(L::RowVersion), "BIGINT");
        let ts = L::Timestamp { precision: None, tz: false };
        assert_eq!(ph().render_default(&DefaultValue::CurrentTimestamp, &ts), None);
        assert_eq!(ph().render_default(&DefaultValue::Text("a".into()), &L::Text { unicode: true }).as_deref(), Some("'a'"));
        assert_eq!(ph().render_default(&DefaultValue::NewUuid, &L::Uuid), None);
    }

    #[test]
    fn finalize_adds_a_row_key_and_relaxes_not_null() {
        let mut t = TableSchema {
            name: "T".into(),
            columns: vec![ColumnDef { name: "A".into(), data_type: "INTEGER".into(), nullable: false, ..Default::default() }],
            indexes: vec![dbine_driver::IndexDef { name: "UX".into(), columns: vec!["A".into()], unique: true, ..Default::default() }],
            ..Default::default()
        };
        let mut rep = Report::default();
        ph().finalize(&mut t, &mut rep);
        assert_eq!(t.columns[0].name, ROW_ID);
        assert_eq!(t.primary_key.as_ref().unwrap().columns, vec![ROW_ID.to_string()]);
        assert!(t.columns[1].nullable);
        assert!(!t.indexes[0].unique);

        let mut rep = Report::default();
        lookup("avatica").unwrap().finalize(&mut t, &mut rep);
        assert!(rep.issues.iter().any(|i| i.severity == Severity::Dropped));
    }
}
