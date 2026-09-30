//! Graph engines: Neo4j, Memgraph and Neptune (Cypher), and OrientDB.
//!
//! Cypher engines: a table is a node label and its columns are the nodes'
//! properties. `database_schema` reports each label (and relationship
//! type) with the property types of a sample (`STRING`, `INTEGER`,
//! `FLOAT`, `BOOLEAN`, `LIST`, `MAP`, `POINT`, several joined with `|`)
//! and its indexes and constraints. Nothing declares a property's type, so
//! the rendered types only document the values; what reaches the target
//! is the indexes: the primary key becomes a uniqueness constraint, unique
//! indexes too, the rest range indexes. Properties can't hold maps, so
//! JSON is text. Foreign keys aren't turned into relationships at this
//! stage: they're dropped and reported. Neptune has no user-defined
//! indexes or constraints, so it receives nothing but the report.
//!
//! OrientDB: a table is a (document) class with a `CREATE PROPERTY` per
//! column, NOT NULL, defaults and indexes, the primary key as a unique
//! index. Its links are record ids, not the values a foreign key holds, so
//! foreign keys are dropped.

use super::mongodb::parse_list;
use super::{Caps, Dialect, IdentCase, Rendered, ALL_ACTIONS};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{IndexDef, TableSchema};

#[derive(Clone, Copy, PartialEq)]
enum Flavor {
    Neo4j,
    Memgraph,
    Neptune,
}

pub struct Cypher {
    flavor: Flavor,
}

pub struct OrientDb;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static NEO4J: Cypher = Cypher { flavor: Flavor::Neo4j };
    static MEMGRAPH: Cypher = Cypher { flavor: Flavor::Memgraph };
    static NEPTUNE: Cypher = Cypher { flavor: Flavor::Neptune };
    static ORIENT: OrientDb = OrientDb;
    match driver_id {
        "neo4j" => Some(&NEO4J),
        "memgraph" => Some(&MEMGRAPH),
        "neptune" => Some(&NEPTUNE),
        "orientdb" => Some(&ORIENT),
        _ => None,
    }
}

// ---- Cypher -----------------------------------------------------------------

fn cypher_type(t: &str) -> L {
    let t = t.trim();
    if let Some(inner) = t.strip_prefix("list<").and_then(|r| r.strip_suffix('>')) {
        return L::Array { of: Box::new(cypher_type(inner)) };
    }
    match t {
        "boolean" => L::Bool,
        "integer" => L::int(8),
        "float" => L::Float { bytes: 8 },
        "string" => L::Text { unicode: true },
        "date" => L::Date,
        "local time" => L::Time { precision: Some(9), tz: false },
        "zoned time" => L::Time { precision: Some(9), tz: true },
        "local datetime" => L::Timestamp { precision: Some(9), tz: false },
        "zoned datetime" => L::Timestamp { precision: Some(9), tz: true },
        "duration" => L::Interval,
        "point" => L::Geometry { kind: Some("point".into()), srid: None, geography: false },
        "list" | "map" | "any" => L::Json { binary: true },
        _ => L::Other { native: t.to_string() },
    }
}

impl Cypher {
    fn render(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } if L::signed_bytes_for(*bytes, *unsigned) <= 8 => Rendered::exact("INTEGER"),
            L::Int { .. } => Rendered::exact("FLOAT").with(Loss, RangeLoss, "Los enteros son de 8 bytes: el valor queda como FLOAT y pierde precisión pasados los 15 dígitos."),
            L::Decimal { precision: Some(p), .. } if *p <= 15 => Rendered::exact("FLOAT").with(Info, TypeChanged, "No hay decimales exactos: FLOAT, exacto hasta 15 dígitos."),
            L::Decimal { .. } => Rendered::exact("FLOAT").with(Loss, PrecisionLoss, "No hay decimales exactos: FLOAT pierde precisión pasados los 15 dígitos."),
            L::Float { .. } => Rendered::exact("FLOAT"),
            L::Money => Rendered::exact("FLOAT").with(Info, TypeChanged, "Moneda como FLOAT."),
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } | L::Uuid | L::Inet | L::MacAddr => Rendered::exact("STRING"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("LIST<INTEGER>").with(Info, TypeChanged, "Binario como arreglo de bytes."),
            L::Bit { .. } => Rendered::exact("STRING").with(Warning, TypeApproximated, "Cadena de bits como texto de 0 y 1."),
            L::Date => Rendered::exact("DATE"),
            L::Time { tz, .. } => Rendered::exact(if *tz { "ZONED TIME" } else { "LOCAL TIME" }),
            L::Timestamp { tz, .. } => Rendered::exact(if *tz { "ZONED DATETIME" } else { "LOCAL DATETIME" }),
            L::Interval => Rendered::exact("DURATION"),
            L::Year => Rendered::exact("INTEGER"),
            L::Json { .. } | L::Map { .. } => Rendered::exact("STRING").with(Warning, TypeApproximated, "Las propiedades no guardan mapas: el JSON queda como texto."),
            L::Xml => Rendered::exact("STRING"),
            L::Enum { values } => Rendered::exact("STRING").with(Info, TypeApproximated, format!("Enumerado como texto. Valores: {}.", values.join(", "))),
            L::Set { .. } => Rendered::exact("LIST<STRING>"),
            L::Array { of } => match **of {
                L::Array { .. } | L::Json { .. } | L::Map { .. } => {
                    Rendered::exact("STRING").with(Warning, TypeApproximated, "Las propiedades no guardan listas anidadas: queda como texto JSON.")
                }
                _ => {
                    let inner = self.render(of);
                    Rendered { native: format!("LIST<{}>", inner.native), notes: inner.notes }
                }
            },
            L::Geometry { kind: Some(k), .. } if k == "point" => Rendered::exact("POINT"),
            L::Geometry { .. } => Rendered::exact("STRING").with(Warning, TypeApproximated, "Solo hay puntos espaciales: la geometría queda como texto (WKT)."),
            L::RowVersion => Rendered::exact("STRING").with(Warning, TypeApproximated, "No hay versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }
}

impl Dialect for Cypher {
    fn id(&self) -> &'static str {
        "neo4j"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let raw = t.raw.trim();
        if raw.is_empty() {
            return L::Json { binary: true };
        }
        match parse_list(raw, cypher_type) {
            L::Other { .. } => L::Other { native: raw.to_string() },
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
            // Let foreign keys through to `finalize`, which reports them as
            // relationships not created (and drops them).
            foreign_keys: true,
            on_delete: ALL_ACTIONS,
            on_update: ALL_ACTIONS,
            indexes: self.flavor != Flavor::Neptune,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: false,
            nullability: false,
            comments: false,
            max_identifier: 255,
            case: IdentCase::Preserve,
        }
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        if t.kind == "label" || t.kind == "relationship" {
            return;
        }
        let name = t.name.clone();
        t.kind = "label".into();
        report.push(
            Severity::Info,
            IssueCode::TableChanged,
            &name,
            None,
            "La tabla pasa a ser una etiqueta de nodos y sus columnas, propiedades: los tipos indican cómo quedan los valores, nada los controla.",
        );
        for fk in std::mem::take(&mut t.foreign_keys) {
            let label = fk.name.clone().unwrap_or_else(|| format!("→ {}", fk.ref_table));
            report.push(
                Severity::Dropped,
                IssueCode::ForeignKeyDropped,
                &name,
                Some(&label),
                format!(
                    "La clave foránea hacia «{}» no se convierte en relación: las propiedades ({}) quedan con sus valores y la relación se puede crear después con MATCH … CREATE.",
                    fk.ref_table,
                    fk.columns.join(", ")
                ),
            );
        }
        let pk = t.primary_key.take().filter(|k| !k.columns.is_empty());
        if self.flavor == Flavor::Neptune {
            if pk.is_some() {
                report.push(Severity::Info, IssueCode::PrimaryKeyDropped, &name, None, "Neptune no tiene restricciones: la clave primaria no se controla.");
            }
            return;
        }
        if let Some(pk) = pk {
            if !t.indexes.iter().any(|i| i.unique && i.columns == pk.columns) {
                let ix = pk.name.clone().unwrap_or_else(|| format!("{name}_pk"));
                report.push(
                    Severity::Info,
                    IssueCode::PrimaryKeyDropped,
                    &name,
                    Some(&ix),
                    format!("La clave primaria ({}) pasa a una restricción de unicidad; que no sea nula no se controla.", pk.columns.join(", ")),
                );
                t.indexes.insert(0, IndexDef { name: ix, columns: pk.columns, unique: true, kind: Some("UNIQUE".into()), filter: None, ..Default::default() });
            }
        }
        for ix in &mut t.indexes {
            if ix.kind.is_none() {
                ix.kind = Some(if ix.unique { "UNIQUE" } else { "RANGE" }.into());
            }
        }
    }
}

// ---- OrientDB ---------------------------------------------------------------

fn orient_type(t: &str) -> L {
    match t {
        "boolean" => L::Bool,
        "byte" => L::int(1),
        "short" => L::int(2),
        "integer" => L::int(4),
        "long" => L::int(8),
        "float" => L::Float { bytes: 4 },
        "double" => L::Float { bytes: 8 },
        "decimal" => L::Decimal { precision: None, scale: None },
        "string" => L::Text { unicode: true },
        "date" => L::Date,
        // Milliseconds since the epoch, shown in the database's time zone.
        "datetime" => L::Timestamp { precision: Some(3), tz: true },
        "binary" | "custom" => L::Blob,
        // A record id (`#12:3`).
        "link" => L::Varchar { len: Some(32), unicode: false },
        "embedded" | "embeddedlist" | "embeddedset" | "embeddedmap" | "linklist" | "linkset" | "linkmap" | "linkbag" | "any" => {
            L::Json { binary: true }
        }
        _ => L::Other { native: t.to_string() },
    }
}

impl Dialect for OrientDb {
    fn id(&self) -> &'static str {
        "orientdb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let raw = t.raw.trim();
        if raw.is_empty() {
            return L::Json { binary: true };
        }
        match parse_list(raw, orient_type) {
            L::Other { .. } => L::Other { native: raw.to_string() },
            l => l,
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => {
                let r = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 => Rendered::exact("BYTE"),
                    2 => Rendered::exact("SHORT"),
                    3 | 4 => Rendered::exact("INTEGER"),
                    8 => Rendered::exact("LONG"),
                    _ => Rendered::exact("DECIMAL"),
                };
                if *unsigned {
                    r.with(Info, TypeChanged, "OrientDB no tiene enteros sin signo: se usa un tipo más grande con signo.")
                } else {
                    r
                }
            }
            L::Decimal { .. } | L::Money => Rendered::exact("DECIMAL"),
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Char { len: Some(_), .. } | L::Varchar { len: Some(_), .. } => Rendered::exact("STRING").with(Info, TypeChanged, "STRING no limita el largo."),
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } | L::Uuid | L::Xml | L::Inet | L::MacAddr => Rendered::exact("STRING"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("BINARY"),
            L::Bit { .. } => Rendered::exact("STRING").with(Warning, TypeApproximated, "Cadena de bits como texto de 0 y 1."),
            L::Date => Rendered::exact("DATE"),
            L::Time { .. } => Rendered::exact("STRING").with(Warning, TypeApproximated, "OrientDB no tiene hora del día: queda como texto."),
            L::Timestamp { precision, tz } => {
                let r = Rendered::exact("DATETIME");
                let r = match precision {
                    Some(p) if *p > 3 => r.with(Loss, PrecisionLoss, format!("DATETIME guarda milisegundos: se pierden {} decimales de segundo.", p - 3)),
                    _ => r,
                };
                if *tz {
                    r
                } else {
                    r.with(Warning, TimeZoneLoss, "DATETIME es un instante: la fecha y hora sin zona se toma en la zona horaria de la base.")
                }
            }
            L::Interval => Rendered::exact("STRING").with(Warning, TypeApproximated, "OrientDB no tiene intervalos: queda como texto."),
            L::Year => Rendered::exact("SHORT").with(Info, TypeChanged, "Año como SHORT."),
            L::Json { .. } => Rendered::exact("EMBEDDED").with(Info, TypeChanged, "JSON como documento embebido."),
            L::Enum { values } => Rendered::exact("STRING").with(Info, TypeApproximated, format!("Enumerado como texto. Valores: {}.", values.join(", "))),
            L::Set { .. } => Rendered::exact("EMBEDDEDSET"),
            L::Array { .. } => Rendered::exact("EMBEDDEDLIST"),
            L::Map { .. } => Rendered::exact("EMBEDDEDMAP"),
            L::Geometry { .. } => Rendered::exact("STRING").with(Warning, TypeApproximated, "Dato espacial como texto (WKT)."),
            L::RowVersion => Rendered::exact("BINARY").with(Warning, TypeApproximated, "OrientDB no tiene versión de fila automática: no se actualiza sola."),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    /// The driver quotes the default; OrientDB converts it to the
    /// property's type and evaluates functions such as `sysdate()`.
    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::Number(n) => n.clone(),
            DefaultValue::Text(s) => s.clone(),
            DefaultValue::Bool(b) => b.to_string(),
            DefaultValue::CurrentTimestamp | DefaultValue::CurrentDate if matches!(ty, L::Date | L::Timestamp { .. }) => "sysdate()".into(),
            _ => return None,
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
            comments: false,
            max_identifier: 128,
            case: IdentCase::Preserve,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn parses_cypher_types() {
        let d = lookup("neo4j").unwrap();
        let p = |s: &str| d.parse_type(&parse(s));
        assert_eq!(p("STRING"), L::Text { unicode: true });
        assert_eq!(p("INTEGER"), L::int(8));
        assert_eq!(p("INTEGER|FLOAT"), L::Float { bytes: 8 });
        assert_eq!(p("BOOLEAN"), L::Bool);
        assert_eq!(p("LIST"), L::Json { binary: true });
        assert_eq!(p("MAP"), L::Json { binary: true });
        assert_eq!(p("LIST<INTEGER>"), L::Array { of: Box::new(L::int(8)) });
        assert_eq!(p("ZONED DATETIME"), L::Timestamp { precision: Some(9), tz: true });
        assert!(matches!(p("POINT"), L::Geometry { .. }));
        assert_eq!(p("NULL"), L::Text { unicode: true });
        assert_eq!(p("STRING|INTEGER"), L::Json { binary: true });
    }

    #[test]
    fn renders_cypher_variants() {
        let d = lookup("neo4j").unwrap();
        let r = |t: L| d.render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(4)), "INTEGER");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "FLOAT");
        assert_eq!(r(L::Decimal { precision: Some(10), scale: Some(2) }), "FLOAT");
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Money), "FLOAT");
        assert_eq!(r(L::Varchar { len: Some(2), unicode: true }), "STRING");
        assert_eq!(r(L::Blob), "LIST<INTEGER>");
        assert_eq!(r(L::Bit { len: None }), "STRING");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: None, tz: true }), "ZONED TIME");
        assert_eq!(r(L::Timestamp { precision: None, tz: false }), "LOCAL DATETIME");
        assert_eq!(r(L::Interval), "DURATION");
        assert_eq!(r(L::Year), "INTEGER");
        assert_eq!(r(L::Uuid), "STRING");
        assert_eq!(r(L::Json { binary: true }), "STRING");
        assert_eq!(r(L::Xml), "STRING");
        assert_eq!(r(L::Enum { values: vec![] }), "STRING");
        assert_eq!(r(L::Set { values: vec![] }), "LIST<STRING>");
        assert_eq!(r(L::Array { of: Box::new(L::int(4)) }), "LIST<INTEGER>");
        assert_eq!(r(L::Array { of: Box::new(L::Array { of: Box::new(L::int(4)) }) }), "STRING");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "STRING");
        assert_eq!(r(L::Geometry { kind: Some("point".into()), srid: None, geography: false }), "POINT");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "STRING");
        assert_eq!(r(L::Inet), "STRING");
        assert_eq!(r(L::MacAddr), "STRING");
        assert_eq!(r(L::RowVersion), "STRING");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
        assert!(!lookup("neptune").unwrap().caps().indexes);
    }

    #[test]
    fn orientdb_types() {
        let d = lookup("orientdb").unwrap();
        let p = |s: &str| d.parse_type(&parse(s));
        assert_eq!(p("STRING"), L::Text { unicode: true });
        assert_eq!(p("LONG"), L::int(8));
        assert_eq!(p("LONG|DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(p("DATETIME"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(p("EMBEDDEDLIST"), L::Json { binary: true });
        assert_eq!(p("LINK"), L::Varchar { len: Some(32), unicode: false });
        let r = |t: L| d.render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(1)), "BYTE");
        assert_eq!(r(L::int(2)), "SHORT");
        assert_eq!(r(L::int(4)), "INTEGER");
        assert_eq!(r(L::int(8)), "LONG");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "DECIMAL");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "DECIMAL");
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Float { bytes: 8 }), "DOUBLE");
        assert_eq!(r(L::Money), "DECIMAL");
        assert_eq!(r(L::Text { unicode: true }), "STRING");
        assert_eq!(r(L::Blob), "BINARY");
        assert_eq!(r(L::Bit { len: None }), "STRING");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: None, tz: false }), "STRING");
        assert_eq!(r(L::Timestamp { precision: None, tz: true }), "DATETIME");
        assert_eq!(r(L::Interval), "STRING");
        assert_eq!(r(L::Year), "SHORT");
        assert_eq!(r(L::Uuid), "STRING");
        assert_eq!(r(L::Json { binary: true }), "EMBEDDED");
        assert_eq!(r(L::Xml), "STRING");
        assert_eq!(r(L::Enum { values: vec![] }), "STRING");
        assert_eq!(r(L::Set { values: vec![] }), "EMBEDDEDSET");
        assert_eq!(r(L::Array { of: Box::new(L::Bool) }), "EMBEDDEDLIST");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "EMBEDDEDMAP");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "STRING");
        assert_eq!(r(L::Inet), "STRING");
        assert_eq!(r(L::MacAddr), "STRING");
        assert_eq!(r(L::RowVersion), "BINARY");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
        assert_eq!(d.render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: true }).as_deref(), Some("sysdate()"));
    }
}
