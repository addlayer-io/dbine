//! ClickHouse and Timeplus (Proton). Same type system; Timeplus spells the
//! names in snake case (`uint64`, `fixed_string(16)`, `low_cardinality`)
//! and has streams instead of MergeTree tables.
//!
//! What matters when a table comes from elsewhere:
//! - no foreign keys, no unique constraints, no auto-increment;
//! - a MergeTree table needs `ENGINE` and `ORDER BY` (derived from the
//!   primary key, `tuple()` without one), and sorting-key columns can't be
//!   `Nullable`;
//! - `DateTime64` is an instant (stored as UTC) from 1900 to 2299, `Date32`
//!   covers the same years;
//! - string literals take backslash escapes.

use super::postgres::precision_loss;
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::convert::logical_of;
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{self, TypeSpec};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::TableSchema;

pub struct ClickHouse {
    timeplus: bool,
}

/// Largest `Decimal(P, S)` precision.
const MAX_DECIMAL: u32 = 76;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static CH: ClickHouse = ClickHouse { timeplus: false };
    static TP: ClickHouse = ClickHouse { timeplus: true };
    match driver_id {
        "clickhouse" => Some(&CH),
        "timeplus" => Some(&TP),
        _ => None,
    }
}

/// A ClickHouse string literal: backslash is an escape character.
pub(super) fn ch_literal(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

impl ClickHouse {
    /// The type name in this flavor's spelling (`DateTime64` → `datetime64`).
    fn n(&self, name: &str) -> String {
        if !self.timeplus {
            return name.to_string();
        }
        match name {
            "FixedString" => "fixed_string".into(),
            "LowCardinality" => "low_cardinality".into(),
            other => other.to_ascii_lowercase(),
        }
    }

    fn inner(&self, arg: &str) -> L {
        logical_of(self, &parse::parse(arg))
    }

    fn int(&self, bytes: u8, unsigned: bool) -> Rendered {
        let bits = match bytes {
            1 => 8,
            2 => 16,
            3 | 4 => 32,
            8 => 64,
            _ => 128,
        };
        let r = Rendered::exact(self.n(&format!("{}Int{bits}", if unsigned { "U" } else { "" })));
        if bytes == 3 {
            r.with(Severity::Info, IssueCode::TypeChanged, "Entero de 3 bytes como entero de 4.")
        } else {
            r
        }
    }

    fn string(&self) -> String {
        self.n("String")
    }

    fn enum_of(&self, values: &[String]) -> String {
        let items: Vec<String> = values.iter().enumerate().map(|(i, v)| format!("{} = {}", ch_literal(v), i + 1)).collect();
        let kind = if values.len() < 128 { "Enum8" } else { "Enum16" };
        format!("{}({})", self.n(kind), items.join(", "))
    }
}

impl Dialect for ClickHouse {
    fn id(&self) -> &'static str {
        if self.timeplus {
            "timeplus"
        } else {
            "clickhouse"
        }
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let name = t.name.as_str();
        match name {
            "bool" | "boolean" => L::Bool,
            "int8" | "tinyint" | "int1" | "byte" => L::int(1),
            "int16" | "smallint" => L::int(2),
            "int32" | "int" | "integer" | "mediumint" => L::int(4),
            "int64" | "bigint" => L::int(8),
            "int128" => L::int(16),
            "uint8" => L::Int { bytes: 1, unsigned: true },
            "uint16" => L::Int { bytes: 2, unsigned: true },
            "uint32" => L::Int { bytes: 4, unsigned: true },
            "uint64" => L::Int { bytes: 8, unsigned: true },
            "uint128" => L::Int { bytes: 16, unsigned: true },
            // 256-bit integers: as many digits as they hold.
            "int256" => L::Decimal { precision: Some(77), scale: Some(0) },
            "uint256" => L::Decimal { precision: Some(78), scale: Some(0) },
            "float32" | "float" | "real" | "bfloat16" => L::Float { bytes: 4 },
            "float64" | "double" => L::Float { bytes: 8 },
            "decimal" | "numeric" => L::Decimal { precision: p(0).or(Some(10)), scale: p(1).or(Some(0)) },
            "decimal32" => L::Decimal { precision: Some(9), scale: p(0) },
            "decimal64" => L::Decimal { precision: Some(18), scale: p(0) },
            "decimal128" => L::Decimal { precision: Some(38), scale: p(0) },
            "decimal256" => L::Decimal { precision: Some(76), scale: p(0) },
            "string" | "text" | "varchar" | "char" | "clob" | "blob" => L::Text { unicode: true },
            "fixedstring" | "fixed_string" => L::Char { len: p(0), unicode: false },
            "uuid" => L::Uuid,
            "date" | "date32" => L::Date,
            // DateTime is an instant (seconds since the epoch) shown in the
            // column's or the server's time zone.
            "datetime" | "timestamp" => L::Timestamp { precision: Some(0), tz: true },
            "datetime64" => L::Timestamp { precision: Some(p(0).unwrap_or(3) as u8), tz: true },
            "time" => L::Time { precision: Some(0), tz: false },
            "time64" => L::Time { precision: Some(p(0).unwrap_or(3) as u8), tz: false },
            "ipv4" | "ipv6" => L::Inet,
            "json" | "object" => L::Json { binary: true },
            "enum8" | "enum16" | "enum" => L::Enum { values: t.args.clone() },
            "array" => match t.args.first() {
                Some(a) => L::Array { of: Box::new(self.inner(a)) },
                None => L::Other { native: t.raw.clone() },
            },
            "map" if t.args.len() == 2 => L::Map { key: Box::new(self.inner(&t.args[0])), value: Box::new(self.inner(&t.args[1])) },
            // Named or positional tuples hold a record: closest is a document.
            "tuple" => L::Json { binary: true },
            // Timeplus spells the wrapper in snake case, which the parser
            // doesn't peel.
            "low_cardinality" | "nullable" if t.args.len() == 1 => self.inner(&t.args[0]),
            "point" => L::Geometry { kind: Some("point".into()), srid: None, geography: false },
            "ring" | "linestring" => L::Geometry { kind: Some("linestring".into()), srid: None, geography: false },
            "multilinestring" => L::Geometry { kind: Some("multilinestring".into()), srid: None, geography: false },
            "polygon" => L::Geometry { kind: Some("polygon".into()), srid: None, geography: false },
            "multipolygon" => L::Geometry { kind: Some("multipolygon".into()), srid: None, geography: false },
            _ if name.starts_with("interval") => L::Interval,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let s = || self.string();
        match t {
            L::Bool => Rendered::exact(self.n("Bool")),
            L::Int { bytes, unsigned } => self.int(*bytes, *unsigned),
            L::Decimal { precision: Some(p), scale } => {
                let sc = scale.unwrap_or(0);
                if *p <= MAX_DECIMAL {
                    Rendered::exact(format!("{}({p}, {sc})", self.n("Decimal")))
                } else if sc == 0 && *p <= 77 {
                    Rendered::exact(self.n("Int256"))
                } else if sc == 0 {
                    Rendered::exact(self.n("Int256"))
                        .with(Loss, RangeLoss, format!("Entero de {p} dígitos como Int256: no entran los valores mayores a 5,7 × 10^76."))
                } else {
                    Rendered::exact(format!("{}({MAX_DECIMAL}, {})", self.n("Decimal"), sc.min(MAX_DECIMAL)))
                        .with(Loss, PrecisionLoss, format!("ClickHouse admite hasta Decimal(76, S); el origen tiene {p} dígitos."))
                }
            }
            L::Decimal { precision: None, .. } => Rendered::exact(format!("{}(76, 20)", self.n("Decimal")))
                .with(Loss, PrecisionLoss, "El origen no fija la precisión: se usa Decimal(76, 20), el mayor de ClickHouse."),
            L::Float { bytes: 4 } => Rendered::exact(self.n("Float32")),
            L::Float { .. } => Rendered::exact(self.n("Float64")),
            L::Money => Rendered::exact(format!("{}(19, 4)", self.n("Decimal"))).with(Info, TypeChanged, "Moneda como Decimal(19, 4)."),
            // FixedString counts bytes: only right for single-byte text.
            L::Char { len: Some(n), unicode: false } => Rendered::exact(format!("{}({n})", self.n("FixedString")))
                .with(Info, TypeChanged, "FixedString rellena con bytes nulos, no con espacios."),
            L::Char { .. } => Rendered::exact(s()).with(Info, TypeChanged, "String no tiene largo fijo."),
            L::Varchar { len: Some(_), .. } => Rendered::exact(s()).with(Info, LengthLoss, "String de ClickHouse no limita el largo."),
            L::Varchar { len: None, .. } | L::Text { .. } => Rendered::exact(s()),
            L::Binary { len: Some(n) } => Rendered::exact(format!("{}({n})", self.n("FixedString"))),
            L::Binary { len: None } | L::Varbinary { .. } | L::Blob => {
                Rendered::exact(s()).with(Info, TypeChanged, "Binario como String (en ClickHouse String guarda bytes).")
            }
            L::Bit { .. } => Rendered::exact(s()).with(Warning, TypeApproximated, "ClickHouse no tiene cadenas de bits: se guarda como texto."),
            L::Date => Rendered::exact(self.n("Date32")).with(Warning, RangeLoss, "Date32 cubre de 1900 a 2299."),
            L::Time { tz, .. } => {
                let r = Rendered::exact(s()).with(Warning, TypeApproximated, "ClickHouse no tiene un tipo hora estable: queda como texto HH:MM:SS.");
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Se pierde la zona horaria de la hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let p = precision.unwrap_or(6);
                let r = Rendered::exact(format!("{}({})", self.n("DateTime64"), p.min(9)))
                    .with_loss(precision_loss(Some(p), 9))
                    .with(Warning, RangeLoss, "DateTime64 cubre de 1900 a 2299.");
                if *tz {
                    r
                } else {
                    r.with(Info, TimeZoneLoss, "DateTime64 guarda instantes: los valores sin zona se interpretan en la zona del servidor.")
                }
            }
            L::Interval => Rendered::exact(s()).with(Warning, TypeApproximated, "ClickHouse no guarda intervalos en columnas: queda como texto."),
            L::Year => Rendered::exact(self.n("UInt16")),
            L::Uuid => Rendered::exact(self.n("UUID")),
            L::Json { .. } => Rendered::exact(self.n("JSON")).with(Info, TypeChanged, "El tipo JSON necesita ClickHouse 24.8 o posterior."),
            L::Xml => Rendered::exact(s()).with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } if !values.is_empty() && values.len() <= 32_767 => Rendered::exact(self.enum_of(values)),
            L::Enum { .. } => Rendered::exact(s()).with(Warning, TypeApproximated, "Enumerado sin valores: queda como texto."),
            L::Set { values } => Rendered::exact(format!("{}({})", self.n("Array"), self.enum_of(values)))
                .with(Warning, TypeApproximated, "Conjunto como arreglo de enumerados."),
            L::Array { of } => {
                let inner = self.render_type(of);
                Rendered { native: format!("{}({})", self.n("Array"), inner.native), notes: inner.notes }
            }
            L::Map { key, value } => {
                let (k, v) = (self.render_type(key), self.render_type(value));
                let mut notes = k.notes;
                notes.extend(v.notes);
                Rendered { native: format!("{}({}, {})", self.n("Map"), k.native, v.native), notes }
            }
            L::Geometry { kind, geography, .. } => {
                let native = match kind.as_deref() {
                    Some("point") => Some("Point"),
                    Some("linestring") => Some("LineString"),
                    Some("multilinestring") => Some("MultiLineString"),
                    Some("polygon") => Some("Polygon"),
                    Some("multipolygon") => Some("MultiPolygon"),
                    _ => None,
                };
                match native {
                    Some(n) => {
                        let r = Rendered::exact(self.n(n)).with(Warning, TypeApproximated, "Los tipos geométricos de ClickHouse no guardan SRID: los datos se cargan como coordenadas.");
                        if *geography { r.with(Info, TypeChanged, "Coordenadas geográficas en un tipo plano.") } else { r }
                    }
                    None => Rendered::exact(s()).with(Warning, TypeApproximated, "Geometría genérica como texto (WKT)."),
                }
            }
            L::Inet => Rendered::exact(s()).with(Info, TypeApproximated, "Dirección IP como texto (IPv4 / IPv6 no guardan redes ni máscaras)."),
            L::MacAddr => Rendered::exact(s()).with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact(self.n("UInt64")).with(Warning, TypeApproximated, "ClickHouse no tiene versión de fila automática."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        let tp = self.timeplus;
        Some(match d {
            DefaultValue::Null => "NULL".into(),
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => ch_literal(s),
            DefaultValue::Bool(b) if matches!(ty, L::Bool) => if *b { "true" } else { "false" }.into(),
            DefaultValue::Bool(b) => if *b { "1" } else { "0" }.into(),
            DefaultValue::CurrentTimestamp => match ty {
                L::Timestamp { precision, .. } => format!("now64({})", precision.unwrap_or(6).min(9)),
                L::Date => "today()".into(),
                _ => "now()".into(),
            },
            DefaultValue::CurrentDate => "today()".into(),
            DefaultValue::CurrentTime => {
                if tp {
                    "format_datetime(now(), '%H:%i:%S')".into()
                } else {
                    "formatDateTime(now(), '%H:%i:%S')".into()
                }
            }
            DefaultValue::NewUuid => if tp { "uuid()" } else { "generateUUIDv4()" }.into(),
            DefaultValue::NextVal(_) | DefaultValue::Expr(_) => return None,
        })
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: false,
            on_delete: &[],
            on_update: &[],
            indexes: true,
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

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        use IssueCode::*;
        use Severity::*;
        let pk: Vec<String> = t.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default();
        // Sorting-key columns can't be Nullable (without allow_nullable_key).
        for c in t.columns.iter_mut().filter(|c| c.nullable && pk.contains(&c.name)) {
            c.nullable = false;
            report.push(Warning, NullabilityChanged, &t.name, Some(&c.name), "La clave de ordenamiento no admite nulos: la columna queda NOT NULL.");
        }
        // Composite types can't be Nullable: a NULL arrives as the empty value.
        for c in t.columns.iter().filter(|c| c.nullable) {
            let lower = c.data_type.to_ascii_lowercase();
            if ["array(", "map(", "tuple(", "json", "object("].iter().any(|p| lower.starts_with(p)) {
                report.push(Info, NullabilityChanged, &t.name, Some(&c.name), "ClickHouse no admite NULL en arreglos, mapas ni JSON: un nulo queda como valor vacío.");
            }
        }
        // Skipping indexes don't enforce anything.
        for ix in t.indexes.iter_mut() {
            if ix.unique {
                ix.unique = false;
                report.push(Warning, IndexChanged, &t.name, Some(&ix.name), "ClickHouse no tiene índices únicos: queda un índice de salto de datos que no garantiza unicidad.");
            } else if ix.kind.is_none() {
                report.push(Info, IndexChanged, &t.name, Some(&ix.name), "Los índices de ClickHouse son de salto de datos: queda de tipo minmax.");
            }
        }
        let key = || match pk.as_slice() {
            [one] => quote_ident(Quote::Backtick, one),
            many => format!("({})", many.iter().map(|c| quote_ident(Quote::Backtick, c)).collect::<Vec<_>>().join(", ")),
        };
        if self.timeplus {
            // An append stream keeps every row; a key needs a key-value mode.
            if !pk.is_empty() && !t.options.contains_key("mode") {
                t.options.insert("mode".into(), "versioned_kv".into());
                report.push(Info, OptionAdded, &t.name, Some("mode"), "Stream en modo versioned_kv: la clave primaria conserva la última versión de cada fila.");
            }
            return;
        }
        if !t.options.contains_key("engine") {
            t.options.insert("engine".into(), "MergeTree".into());
            report.push(Info, OptionAdded, &t.name, Some("engine"), "Motor MergeTree, el habitual para tablas.");
        }
        let merge_tree = t.options.get("engine").is_some_and(|e| e.contains("MergeTree"));
        if merge_tree && !t.options.contains_key("order_by") {
            let (order, msg) = if pk.is_empty() {
                ("tuple()".to_string(), "Sin clave primaria: ORDER BY tuple() (sin orden). Conviene elegir una clave de ordenamiento.".to_string())
            } else {
                let k = key();
                let msg = format!("ORDER BY {k}, la clave primaria de origen. ClickHouse no la hace única: las filas repetidas se guardan igual.");
                (k, msg)
            };
            t.options.insert("order_by".into(), order);
            report.push(if pk.is_empty() { Warning } else { Info }, OptionAdded, &t.name, Some("order_by"), msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use dbine_driver::{ColumnDef, IndexDef, KeyDef};

    fn ch() -> &'static dyn Dialect {
        lookup("clickhouse").unwrap()
    }

    fn tp() -> &'static dyn Dialect {
        lookup("timeplus").unwrap()
    }

    fn lt(d: &dyn Dialect, s: &str) -> L {
        logical_of(d, &parse(s))
    }

    #[test]
    fn parses_catalog_spellings() {
        let d = ch();
        assert_eq!(lt(d, "UInt64"), L::Int { bytes: 8, unsigned: true });
        assert_eq!(lt(d, "Int128"), L::int(16));
        assert_eq!(lt(d, "UInt256"), L::Decimal { precision: Some(78), scale: Some(0) });
        assert_eq!(lt(d, "Nullable(Decimal(12, 2))"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(lt(d, "Decimal64(4)"), L::Decimal { precision: Some(18), scale: Some(4) });
        assert_eq!(lt(d, "LowCardinality(Nullable(String))"), L::Text { unicode: true });
        assert_eq!(lt(d, "FixedString(3)"), L::Char { len: Some(3), unicode: false });
        assert_eq!(lt(d, "DateTime('Europe/Madrid')"), L::Timestamp { precision: Some(0), tz: true });
        assert_eq!(lt(d, "DateTime64(6, 'UTC')"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(lt(d, "Date32"), L::Date);
        assert_eq!(lt(d, "Bool"), L::Bool);
        assert_eq!(lt(d, "IPv6"), L::Inet);
        assert_eq!(lt(d, "UUID"), L::Uuid);
        assert_eq!(lt(d, "JSON"), L::Json { binary: true });
        assert_eq!(lt(d, "Enum8('a' = 1, 'b' = 2)"), L::Enum { values: vec!["a".into(), "b".into()] });
        assert_eq!(lt(d, "Array(Nullable(Int32))"), L::Array { of: Box::new(L::int(4)) });
        assert_eq!(
            lt(d, "Map(String, Array(UInt8))"),
            L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::Array { of: Box::new(L::Int { bytes: 1, unsigned: true }) }) }
        );
        assert_eq!(lt(d, "Tuple(a String, b Int32)"), L::Json { binary: true });
        assert_eq!(lt(d, "Float32"), L::Float { bytes: 4 });
        assert_eq!(lt(d, "Point"), L::Geometry { kind: Some("point".into()), srid: None, geography: false });
        assert!(matches!(lt(d, "AggregateFunction(uniq, UInt64)"), L::Other { .. }));
    }

    #[test]
    fn parses_timeplus_spellings() {
        let d = tp();
        assert_eq!(lt(d, "low_cardinality(string)"), L::Text { unicode: true });
        assert_eq!(lt(d, "nullable(int32)"), L::int(4));
        assert_eq!(lt(d, "fixed_string(16)"), L::Char { len: Some(16), unicode: false });
        assert_eq!(lt(d, "datetime64(3)"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(lt(d, "array(string)"), L::Array { of: Box::new(L::Text { unicode: true }) });
    }

    #[test]
    fn renders_every_variant() {
        let d = ch();
        let r = |t: L| d.render_type(&t);
        assert_eq!(r(L::Bool).native, "Bool");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }).native, "UInt32");
        assert_eq!(r(L::Int { bytes: 3, unsigned: false }).native, "Int32");
        assert_eq!(r(L::int(16)).native, "Int128");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }).native, "Decimal(12, 2)");
        assert_eq!(r(L::Decimal { precision: Some(77), scale: Some(0) }).native, "Int256");
        assert_eq!(r(L::Decimal { precision: None, scale: None }).notes[0].code, IssueCode::PrecisionLoss);
        assert_eq!(r(L::Float { bytes: 4 }).native, "Float32");
        assert_eq!(r(L::Float { bytes: 8 }).native, "Float64");
        assert_eq!(r(L::Money).native, "Decimal(19, 4)");
        assert_eq!(r(L::Char { len: Some(2), unicode: false }).native, "FixedString(2)");
        assert_eq!(r(L::Char { len: Some(2), unicode: true }).native, "String");
        assert_eq!(r(L::Varchar { len: Some(20), unicode: true }).native, "String");
        assert_eq!(r(L::Text { unicode: true }).native, "String");
        assert_eq!(r(L::Binary { len: Some(16) }).native, "FixedString(16)");
        assert_eq!(r(L::Varbinary { len: Some(16) }).native, "String");
        assert_eq!(r(L::Blob).native, "String");
        assert_eq!(r(L::Bit { len: Some(3) }).native, "String");
        assert_eq!(r(L::Date).native, "Date32");
        assert_eq!(r(L::Time { precision: None, tz: false }).native, "String");
        assert_eq!(r(L::Timestamp { precision: None, tz: true }).native, "DateTime64(6)");
        let ts = r(L::Timestamp { precision: Some(12), tz: false });
        assert_eq!(ts.native, "DateTime64(9)");
        assert!(ts.notes.iter().any(|n| n.code == IssueCode::PrecisionLoss));
        assert_eq!(r(L::Interval).native, "String");
        assert_eq!(r(L::Year).native, "UInt16");
        assert_eq!(r(L::Uuid).native, "UUID");
        assert_eq!(r(L::Json { binary: true }).native, "JSON");
        assert_eq!(r(L::Xml).native, "String");
        assert_eq!(r(L::Enum { values: vec!["a".into(), "it's".into()] }).native, "Enum8('a' = 1, 'it\\'s' = 2)");
        assert_eq!(r(L::Set { values: vec!["x".into()] }).native, "Array(Enum8('x' = 1))");
        assert_eq!(r(L::Array { of: Box::new(L::int(8)) }).native, "Array(Int64)");
        assert_eq!(r(L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::int(4)) }).native, "Map(String, Int32)");
        assert_eq!(r(L::Geometry { kind: Some("point".into()), srid: Some(4326), geography: false }).native, "Point");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }).native, "String");
        assert_eq!(r(L::Inet).native, "String");
        assert_eq!(r(L::MacAddr).native, "String");
        assert_eq!(r(L::RowVersion).native, "UInt64");
        assert_eq!(r(L::Other { native: "Nothing".into() }).native, "Nothing");
    }

    #[test]
    fn timeplus_names() {
        let d = tp();
        assert_eq!(d.render_type(&L::Int { bytes: 8, unsigned: true }).native, "uint64");
        assert_eq!(d.render_type(&L::Timestamp { precision: Some(3), tz: true }).native, "datetime64(3)");
        assert_eq!(d.render_type(&L::Binary { len: Some(4) }).native, "fixed_string(4)");
        assert_eq!(d.render_type(&L::Array { of: Box::new(L::Text { unicode: true }) }).native, "array(string)");
    }

    #[test]
    fn defaults() {
        let d = ch();
        let ts = L::Timestamp { precision: Some(3), tz: true };
        assert_eq!(d.render_default(&DefaultValue::CurrentTimestamp, &ts).as_deref(), Some("now64(3)"));
        assert_eq!(d.render_default(&DefaultValue::CurrentDate, &L::Date).as_deref(), Some("today()"));
        assert_eq!(d.render_default(&DefaultValue::NewUuid, &L::Uuid).as_deref(), Some("generateUUIDv4()"));
        assert_eq!(d.render_default(&DefaultValue::Text("a\\b'c".into()), &L::Text { unicode: true }).as_deref(), Some("'a\\\\b\\'c'"));
        assert_eq!(d.render_default(&DefaultValue::Bool(true), &L::Bool).as_deref(), Some("true"));
        assert_eq!(d.render_default(&DefaultValue::Expr("x + 1".into()), &L::int(4)), None);
    }

    fn table() -> TableSchema {
        TableSchema {
            kind: "table".into(),
            name: "t".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "Int64".into(), nullable: true, ..Default::default() },
                ColumnDef { name: "d".into(), data_type: "Date32".into(), nullable: true, ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into(), "d".into()] }),
            indexes: vec![IndexDef { name: "u".into(), columns: vec!["d".into()], unique: true, ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn finalize_adds_engine_and_order_by() {
        let mut t = table();
        let mut rep = Report::default();
        ch().finalize(&mut t, &mut rep);
        assert_eq!(t.options.get("engine").map(String::as_str), Some("MergeTree"));
        assert_eq!(t.options.get("order_by").map(String::as_str), Some("(`id`, `d`)"));
        assert!(t.columns.iter().all(|c| !c.nullable));
        assert!(!t.indexes[0].unique);
        assert!(rep.issues.iter().any(|i| i.code == IssueCode::OptionAdded && i.object.as_deref() == Some("order_by")));

        let mut t = table();
        t.primary_key = None;
        let mut rep = Report::default();
        ch().finalize(&mut t, &mut rep);
        assert_eq!(t.options.get("order_by").map(String::as_str), Some("tuple()"));

        let mut t = table();
        let mut rep = Report::default();
        tp().finalize(&mut t, &mut rep);
        assert_eq!(t.options.get("mode").map(String::as_str), Some("versioned_kv"));
        assert!(!t.options.contains_key("engine"));
    }
}
