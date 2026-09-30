//! Cassandra, ScyllaDB and Amazon Keyspaces (CQL).
//!
//! Types come from `system_schema.columns` as CQL spells them: scalars
//! (`text`, `bigint`, `timeuuid`…) and generics (`list<text>`,
//! `map<text, frozen<list<int>>>`, `tuple<int, text>`, `vector<float, 3>`).
//! Anything else is a user-defined type, which other engines get as JSON.
//!
//! The designer marks keys per column (`partition_key`, `clustering_key`,
//! `clustering_order`). From SQL, the key is deduced from the primary key
//! the way CQL itself reads `PRIMARY KEY (a, b, c)`: the first column is
//! the partition key and the others are clustering columns, ascending. So
//! the rows that share the key's first column (an order's lines, a
//! tenant's records) live together and come back sorted, and a lookup by
//! that column — the one SQL's leftmost-prefix rule makes cheap too —
//! stays a single-partition read. Without a primary key the first unique
//! index is the key, else every scalar column (rows that are equal in all
//! of them collapse into one; reported).
//!
//! CQL has no foreign keys, NOT NULL, defaults, auto-increment or unique
//! and multi-column indexes: all reported. Keyspaces has no secondary
//! indexes at all.

use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{KeyDef, TableSchema};
use std::collections::HashSet;

pub struct Cql {
    secondary_indexes: bool,
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static CQL: Cql = Cql { secondary_indexes: true };
    static KEYSPACES: Cql = Cql { secondary_indexes: false };
    match driver_id {
        "cassandra" | "scylladb" => Some(&CQL),
        "keyspaces" => Some(&KEYSPACES),
        _ => None,
    }
}

/// `head<inner>` → (`head`, `inner`).
fn generic(s: &str) -> Option<(&str, &str)> {
    let open = s.find('<')?;
    let inner = s.strip_suffix('>')?;
    Some((s[..open].trim(), inner[open + 1..].trim()))
}

/// Split on commas outside `<…>`.
fn split_top(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

fn cql_type(s: &str) -> L {
    let s = s.trim().to_ascii_lowercase();
    if let Some((head, inner)) = generic(&s) {
        return match head {
            "frozen" => cql_type(inner),
            "list" | "set" => L::Array { of: Box::new(cql_type(inner)) },
            "map" => match split_top(inner).as_slice() {
                [k, v] => L::Map { key: Box::new(cql_type(k)), value: Box::new(cql_type(v)) },
                _ => L::Json { binary: true },
            },
            "vector" => L::Array { of: Box::new(split_top(inner).first().map_or(L::Float { bytes: 4 }, |t| cql_type(t))) },
            // Tuples and anything generic we don't know.
            _ => L::Json { binary: true },
        };
    }
    match s.as_str() {
        "boolean" => L::Bool,
        "tinyint" => L::int(1),
        "smallint" => L::int(2),
        "int" => L::int(4),
        "bigint" | "counter" => L::int(8),
        // Arbitrary precision integer.
        "varint" => L::Decimal { precision: None, scale: Some(0) },
        "decimal" => L::Decimal { precision: None, scale: None },
        "float" => L::Float { bytes: 4 },
        "double" => L::Float { bytes: 8 },
        "text" | "varchar" => L::Text { unicode: true },
        "ascii" => L::Text { unicode: false },
        "blob" => L::Blob,
        "date" => L::Date,
        // Nanoseconds; SQL engines keep at most 6-7 digits.
        "time" => L::Time { precision: Some(9), tz: false },
        // Milliseconds since the epoch, UTC.
        "timestamp" => L::Timestamp { precision: Some(3), tz: true },
        "duration" => L::Interval,
        "uuid" | "timeuuid" => L::Uuid,
        "inet" => L::Inet,
        "" => L::Other { native: String::new() },
        // A user-defined type: a structure, as JSON elsewhere.
        _ => L::Json { binary: true },
    }
}

fn is_collection(native: &str) -> bool {
    ["list<", "set<", "map<"].iter().any(|p| native.starts_with(p))
}

/// Collections nested in collections (or in keys) have to be frozen.
fn frozen(native: String) -> String {
    if is_collection(&native) {
        format!("frozen<{native}>")
    } else {
        native
    }
}

impl Cql {
    fn render(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("boolean"),
            L::Int { bytes, unsigned } => {
                let r = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 => Rendered::exact("tinyint"),
                    2 => Rendered::exact("smallint"),
                    3 | 4 => Rendered::exact("int"),
                    8 => Rendered::exact("bigint"),
                    _ => Rendered::exact("varint"),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, "CQL no tiene enteros sin signo: se usa un tipo más grande con signo.")
                } else {
                    r
                }
            }
            // Whole numbers of any size.
            L::Decimal { scale: Some(0), .. } => Rendered::exact("varint"),
            L::Decimal { .. } => Rendered::exact("decimal"),
            L::Float { bytes: 4 } => Rendered::exact("float"),
            L::Float { .. } => Rendered::exact("double"),
            L::Money => Rendered::exact("decimal").with(Info, TypeChanged, "Moneda como decimal."),
            L::Char { len: Some(_), .. } | L::Varchar { len: Some(_), .. } => {
                Rendered::exact("text").with(Info, TypeChanged, "CQL no limita el largo del texto.")
            }
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } => Rendered::exact("text"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("blob"),
            L::Bit { .. } => Rendered::exact("text").with(Warning, TypeApproximated, "Cadena de bits como texto de 0 y 1."),
            L::Date => Rendered::exact("date"),
            L::Time { tz, .. } => {
                let r = Rendered::exact("time");
                if *tz {
                    r.with(Loss, TimeZoneLoss, "CQL no guarda la zona horaria de una hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact("timestamp");
                let r = match precision {
                    // The bulk copy refuses those values rather than truncate them.
                    Some(p) if *p > 3 => r.with(
                        Loss,
                        PrecisionLoss,
                        format!(
                            "timestamp de CQL guarda milisegundos y el origen tiene hasta {p} decimales de segundo: \
                             la copia de datos va a fallar con los valores que tengan dígitos más allá del milisegundo."
                        ),
                    ),
                    _ => r,
                };
                if *tz {
                    r
                } else {
                    r.with(Warning, TimeZoneLoss, "timestamp de CQL es un instante en UTC: la fecha y hora sin zona se toma como UTC.")
                }
            }
            L::Interval => Rendered::exact("duration"),
            L::Year => Rendered::exact("smallint").with(Info, TypeChanged, "Año como smallint."),
            L::Uuid => Rendered::exact("uuid"),
            L::Json { .. } => Rendered::exact("text").with(Warning, TypeApproximated, "CQL no tiene JSON: queda como texto."),
            L::Xml => Rendered::exact("text").with(Info, TypeApproximated, "XML como texto."),
            L::Enum { values } => Rendered::exact("text").with(Info, TypeApproximated, format!("Enumerado como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => Rendered::exact("set<text>").with(Info, TypeApproximated, format!("Conjunto como set<text>. Valores: {}.", values.join(", "))),
            L::Array { of } => {
                let inner = self.render(of);
                Rendered { native: format!("list<{}>", frozen(inner.native)), notes: inner.notes }
            }
            L::Map { key, value } => {
                let (k, v) = (self.render(key), self.render(value));
                let mut notes = k.notes;
                notes.extend(v.notes);
                Rendered { native: format!("map<{}, {}>", frozen(k.native), frozen(v.native)), notes }
            }
            L::Geometry { .. } => Rendered::exact("text").with(Warning, TypeApproximated, "CQL no tiene tipos espaciales: queda como texto (WKT)."),
            L::Inet => Rendered::exact("inet"),
            L::MacAddr => Rendered::exact("text").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("blob").with(Warning, TypeApproximated, "CQL no tiene versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }
}

fn is_true(c: &dbine_driver::ColumnDef, k: &str) -> bool {
    c.options.get(k).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

impl Dialect for Cql {
    fn id(&self) -> &'static str {
        "cassandra"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        match cql_type(&t.raw) {
            L::Other { .. } => L::Other { native: t.raw.clone() },
            l => l,
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        self.render(t)
    }

    fn render_default(&self, _d: &DefaultValue, _ty: &L) -> Option<String> {
        None
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: false,
            on_delete: &[],
            on_update: &[],
            indexes: self.secondary_indexes,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: false,
            nullability: false,
            comments: false,
            max_identifier: 48,
            case: IdentCase::Lower,
        }
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        // Read from a CQL server: the key is already in the column options.
        if t.columns.iter().any(|c| is_true(c, "partition_key")) {
            return;
        }
        let name = t.name.clone();
        t.kind = dbine_driver::kinds::TABLE.into();
        let exists = |c: &String| t.columns.iter().any(|x| &x.name == c);
        let mut key: Vec<String> = t.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default();
        if key.is_empty() {
            if let Some(ix) = t.indexes.iter().find(|i| i.unique && !i.columns.is_empty() && i.columns.iter().all(exists)) {
                key = ix.columns.clone();
                report.push(Severity::Warning, IssueCode::PrimaryKeyAdded, &name, Some(&ix.name), format!("La tabla no tiene clave primaria: se usa la del índice único «{}».", ix.name));
            } else {
                key = t.columns.iter().filter(|c| !is_collection(&c.data_type)).map(|c| c.name.clone()).collect();
                report.push(
                    Severity::Warning,
                    IssueCode::PrimaryKeyAdded,
                    &name,
                    None,
                    "La tabla no tiene clave primaria y CQL la exige: la forman todas las columnas simples, así que las filas iguales en todas ellas quedan en una sola.",
                );
            }
        }
        for (i, k) in key.iter().enumerate() {
            let Some(c) = t.columns.iter_mut().find(|c| &c.name == k) else { continue };
            if i == 0 {
                c.options.insert("partition_key".into(), "true".into());
            } else {
                c.options.insert("clustering_key".into(), "true".into());
                c.options.insert("clustering_order".into(), "ASC".into());
            }
            if is_collection(&c.data_type) {
                c.data_type = frozen(std::mem::take(&mut c.data_type));
            }
        }
        if let Some((first, rest)) = key.split_first() {
            let msg = if rest.is_empty() {
                format!("Clave de partición: {first}.")
            } else {
                format!(
                    "Clave de partición: {first}; columnas de clustering (ascendentes): {}. Las filas con el mismo {first} quedan juntas en una partición: si son muchas, conviene revisar la clave.",
                    rest.join(", ")
                )
            };
            report.push(Severity::Info, IssueCode::OptionAdded, &name, Some("partition_key"), msg);
        }
        t.primary_key = (!key.is_empty()).then(|| KeyDef { name: t.primary_key.as_ref().and_then(|k| k.name.clone()), columns: key.clone() });

        // Secondary indexes: one column, not unique, not the sole partition key.
        let mut indexed: HashSet<String> = HashSet::new();
        let mut kept = Vec::new();
        for ix in std::mem::take(&mut t.indexes) {
            let why = if ix.unique {
                Some("CQL no tiene índices únicos: la unicidad no se controla.")
            } else if ix.columns.len() != 1 {
                Some("Un índice secundario de CQL lleva una sola columna.")
            } else if key.len() == 1 && key[0] == ix.columns[0] {
                Some("La columna ya es la clave de partición.")
            } else if !indexed.insert(ix.columns[0].clone()) {
                Some("Ya hay un índice sobre esa columna.")
            } else {
                None
            };
            match why {
                Some(m) => report.push(Severity::Dropped, IssueCode::IndexDropped, &name, Some(&ix.name), m),
                None => kept.push(ix),
            }
        }
        t.indexes = kept;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn p(s: &str) -> L {
        lookup("cassandra").unwrap().parse_type(&parse(s))
    }

    #[test]
    fn parses_cql_types() {
        assert_eq!(p("text"), L::Text { unicode: true });
        assert_eq!(p("ascii"), L::Text { unicode: false });
        assert_eq!(p("bigint"), L::int(8));
        assert_eq!(p("counter"), L::int(8));
        assert_eq!(p("varint"), L::Decimal { precision: None, scale: Some(0) });
        assert_eq!(p("decimal"), L::Decimal { precision: None, scale: None });
        assert_eq!(p("timestamp"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(p("time"), L::Time { precision: Some(9), tz: false });
        assert_eq!(p("timeuuid"), L::Uuid);
        assert_eq!(p("duration"), L::Interval);
        assert_eq!(p("inet"), L::Inet);
        assert_eq!(p("list<text>"), L::Array { of: Box::new(L::Text { unicode: true }) });
        assert_eq!(p("set<int>"), L::Array { of: Box::new(L::int(4)) });
        assert_eq!(
            p("map<text, frozen<list<int>>>"),
            L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::Array { of: Box::new(L::int(4)) }) }
        );
        assert_eq!(p("frozen<tuple<int, text>>"), L::Json { binary: true });
        assert_eq!(p("vector<float, 3>"), L::Array { of: Box::new(L::Float { bytes: 4 }) });
        assert_eq!(p("direccion"), L::Json { binary: true });
        assert_eq!(p("frozen<direccion>"), L::Json { binary: true });
    }

    #[test]
    fn renders_every_variant() {
        let d = lookup("cassandra").unwrap();
        let r = |t: L| d.render_type(&t).native;
        assert_eq!(r(L::Bool), "boolean");
        assert_eq!(r(L::int(1)), "tinyint");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "bigint");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "varint");
        assert_eq!(r(L::Decimal { precision: Some(10), scale: Some(2) }), "decimal");
        assert_eq!(r(L::Decimal { precision: Some(20), scale: Some(0) }), "varint");
        assert_eq!(r(L::Float { bytes: 4 }), "float");
        assert_eq!(r(L::Float { bytes: 8 }), "double");
        assert_eq!(r(L::Money), "decimal");
        assert_eq!(r(L::Varchar { len: Some(3), unicode: false }), "text");
        assert_eq!(r(L::Blob), "blob");
        assert_eq!(r(L::Bit { len: Some(1) }), "text");
        assert_eq!(r(L::Date), "date");
        assert_eq!(r(L::Time { precision: Some(6), tz: false }), "time");
        assert_eq!(r(L::Timestamp { precision: Some(6), tz: true }), "timestamp");
        assert_eq!(r(L::Interval), "duration");
        assert_eq!(r(L::Year), "smallint");
        assert_eq!(r(L::Uuid), "uuid");
        assert_eq!(r(L::Json { binary: true }), "text");
        assert_eq!(r(L::Xml), "text");
        assert_eq!(r(L::Enum { values: vec![] }), "text");
        assert_eq!(r(L::Set { values: vec![] }), "set<text>");
        assert_eq!(r(L::Array { of: Box::new(L::Array { of: Box::new(L::int(4)) }) }), "list<frozen<list<int>>>");
        assert_eq!(r(L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::int(8)) }), "map<text, bigint>");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "text");
        assert_eq!(r(L::Inet), "inet");
        assert_eq!(r(L::MacAddr), "text");
        assert_eq!(r(L::RowVersion), "blob");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
        assert!(!lookup("keyspaces").unwrap().caps().indexes);
    }

    /// Sub-millisecond digits make the data copy fail (it never truncates
    /// them): the note says so, at the severity of values that don't fit.
    #[test]
    fn finer_timestamps_warn_that_the_copy_fails() {
        let d = lookup("cassandra").unwrap();
        let r = d.render_type(&L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(r.native, "timestamp");
        let [n] = r.notes.as_slice() else { panic!("{:?}", r.notes) };
        assert_eq!((n.severity, n.code), (Severity::Loss, IssueCode::PrecisionLoss));
        assert!(n.message.contains("6 decimales") && n.message.contains("va a fallar"), "{}", n.message);
        assert!(d.render_type(&L::Timestamp { precision: Some(3), tz: true }).notes.is_empty());
        assert!(d.render_type(&L::Timestamp { precision: None, tz: true }).notes.is_empty());
    }
}
