//! Amazon DynamoDB (and DynamoDB Local).
//!
//! Only key attributes (of the table and of its indexes) are declared, with
//! a scalar type: `S` (text), `N` (number, up to 38 digits) or `B`
//! (binary). Every other attribute is free per item; `database_schema`
//! reports them from a sample with the type codes seen (`S`, `N`, `BOOL`,
//! `L`, `M`, `SS`, `NS`, `BS`, `NULL`; several joined with ` | `).
//!
//! From SQL, every column gets the code its values will have, but only the
//! key attributes' types reach `CREATE TABLE`. The key: the primary key's
//! first column is the partition key (`HASH`) and its second, if any, the
//! sort key (`RANGE`); DynamoDB keys have at most two attributes, so a
//! longer key is reported (its uniqueness isn't kept). Secondary indexes
//! are global (GSI), of one or two attributes, never unique.

use super::mongodb::parse_list;
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{KeyDef, TableSchema};

pub struct DynamoDb;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: DynamoDb = DynamoDb;
    (driver_id == "dynamodb").then_some(&D as &dyn Dialect)
}

const KEY_TYPE: &str = "key_type";

fn code(t: &str) -> L {
    match t {
        "s" => L::Text { unicode: true },
        // Up to 38 significant digits, any scale.
        "n" => L::Decimal { precision: None, scale: None },
        "b" => L::Blob,
        "bool" => L::Bool,
        "ss" => L::Array { of: Box::new(L::Text { unicode: true }) },
        "ns" => L::Array { of: Box::new(L::Decimal { precision: None, scale: None }) },
        "bs" => L::Array { of: Box::new(L::Blob) },
        "l" | "m" => L::Json { binary: true },
        _ => L::Other { native: t.to_ascii_uppercase() },
    }
}

fn is_key_type(t: &str) -> bool {
    matches!(t, "S" | "N" | "B")
}

/// Names DynamoDB accepts for tables and indexes: 3 to 255 of
/// `a-z A-Z 0-9 _ - .`.
fn valid_name(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' }).collect();
    while s.len() < 3 {
        s.push('_');
    }
    s.truncate(255);
    s
}

impl Dialect for DynamoDb {
    fn id(&self) -> &'static str {
        "dynamodb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let raw = t.raw.trim();
        if raw.is_empty() {
            return L::Json { binary: true };
        }
        match parse_list(raw, code) {
            L::Other { .. } => L::Other { native: raw.to_string() },
            l => l,
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("BOOL"),
            L::Int { .. } | L::Year => Rendered::exact("N"),
            L::Decimal { precision: Some(p), .. } if *p <= 38 => Rendered::exact("N"),
            L::Decimal { .. } => Rendered::exact("N").with(Loss, PrecisionLoss, "Los números de DynamoDB tienen hasta 38 dígitos significativos."),
            L::Float { bytes: 4 } => Rendered::exact("N").with(Info, TypeChanged, "Número decimal: DynamoDB no guarda NaN ni infinitos."),
            // N spans 1E-130 to 9.99E+125; a double reaches 1.8E+308.
            L::Float { .. } => Rendered::exact("N").with(Loss, RangeLoss, "Los números de DynamoDB van de 1E-130 a 9.99E+125 (sin NaN ni infinitos): los double fuera de ese rango no entran."),
            L::Money => Rendered::exact("N"),
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } | L::Uuid | L::Xml | L::Inet | L::MacAddr => Rendered::exact("S"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("B"),
            L::Bit { .. } => Rendered::exact("S").with(Warning, TypeApproximated, "Cadena de bits como texto de 0 y 1."),
            L::Date | L::Time { .. } | L::Timestamp { .. } | L::Interval => {
                Rendered::exact("S").with(Info, TypeChanged, "DynamoDB no tiene fechas: el valor va como texto ISO 8601.")
            }
            L::Json { .. } => Rendered::exact("M").with(Info, TypeChanged, "JSON como mapa (M) o lista (L)."),
            L::Map { .. } => Rendered::exact("M"),
            L::Enum { values } => Rendered::exact("S").with(Info, TypeApproximated, format!("Enumerado como texto. Valores: {}.", values.join(", "))),
            L::Set { .. } => Rendered::exact("SS"),
            L::Array { .. } => Rendered::exact("L"),
            L::Geometry { .. } => Rendered::exact("S").with(Warning, TypeApproximated, "DynamoDB no tiene tipos espaciales: queda como texto (WKT)."),
            L::RowVersion => Rendered::exact("B").with(Warning, TypeApproximated, "DynamoDB no tiene versión de fila automática: no se actualiza sola."),
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
            indexes: true,
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
        let marked = |v: &str| v.eq_ignore_ascii_case("HASH") || v.eq_ignore_ascii_case("RANGE");
        if t.columns.iter().any(|c| c.options.get(KEY_TYPE).is_some_and(|v| marked(v))) {
            return;
        }
        let source = t.name.clone();
        t.kind = dbine_driver::kinds::TABLE.into();
        let name = valid_name(&t.name);
        if name != t.name {
            report.push(Severity::Info, IssueCode::IdentifierRenamed, &source, Some(&source), format!("Nombre de tabla válido para DynamoDB: «{name}»."));
            t.name = name;
        }
        let mut key: Vec<String> = t.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default();
        if key.is_empty() {
            if let Some(ix) = t.indexes.iter().find(|i| i.unique && !i.columns.is_empty()) {
                key = ix.columns.clone();
                report.push(Severity::Warning, IssueCode::PrimaryKeyAdded, &source, Some(&ix.name), format!("La tabla no tiene clave primaria: se usa la del índice único «{}».", ix.name));
            } else if let Some(c) = t.columns.first() {
                key = vec![c.name.clone()];
                report.push(
                    Severity::Warning,
                    IssueCode::PrimaryKeyAdded,
                    &source,
                    Some(&c.name),
                    format!("La tabla no tiene clave primaria y DynamoDB la exige: se usa «{}», y las filas con el mismo valor quedan en un solo ítem.", c.name),
                );
            }
        }
        if key.len() > 2 {
            report.push(
                Severity::Loss,
                IssueCode::PrimaryKeyDropped,
                &source,
                None,
                format!(
                    "La clave de DynamoDB tiene hasta dos atributos: queda ({}, {}) y la unicidad de ({}) no se controla.",
                    key[0],
                    key[1],
                    key.join(", ")
                ),
            );
            key.truncate(2);
        }
        let retype = |col: &str, t: &mut TableSchema, report: &mut Report| {
            if let Some(c) = t.columns.iter_mut().find(|c| c.name == col) {
                if !is_key_type(&c.data_type) {
                    report.push(Severity::Warning, IssueCode::TypeChanged, &source, Some(col), format!("Un atributo clave es S, N o B: «{}» queda como S.", c.data_type));
                    c.data_type = "S".into();
                }
            }
        };
        for c in &mut t.columns {
            let kt = match key.iter().position(|k| *k == c.name) {
                Some(0) => "HASH",
                Some(_) => "RANGE",
                None => "none",
            };
            c.options.insert(KEY_TYPE.into(), kt.into());
        }
        for k in key.clone() {
            retype(&k, t, report);
        }
        if let Some((h, r)) = key.split_first() {
            let msg = match r.first() {
                Some(r) => format!("Clave de partición (HASH): {h}; de ordenación (RANGE): {r}."),
                None => format!("Clave de partición (HASH): {h}."),
            };
            report.push(Severity::Info, IssueCode::OptionAdded, &source, Some(KEY_TYPE), msg);
        }
        t.primary_key = (!key.is_empty()).then(|| KeyDef { name: None, columns: key.clone() });

        let mut kept = Vec::new();
        for mut ix in std::mem::take(&mut t.indexes) {
            if ix.columns == key {
                report.push(Severity::Info, IssueCode::IndexDropped, &source, Some(&ix.name), "El índice repite la clave de la tabla.");
                continue;
            }
            let bad = ix.columns.iter().take(2).find(|c| t.columns.iter().find(|x| &x.name == *c).is_none_or(|x| !is_key_type(&x.data_type)));
            if let Some(c) = bad {
                report.push(
                    Severity::Dropped,
                    IssueCode::IndexDropped,
                    &source,
                    Some(&ix.name),
                    format!("La clave de un índice es S, N o B, y «{c}» no lo es."),
                );
                continue;
            }
            if ix.unique {
                ix.unique = false;
                report.push(Severity::Warning, IssueCode::IndexChanged, &source, Some(&ix.name), "DynamoDB no tiene índices únicos: queda un índice global común.");
            }
            if ix.columns.len() > 2 {
                report.push(Severity::Warning, IssueCode::IndexChanged, &source, Some(&ix.name), format!("Un índice global tiene hasta dos atributos: quedan {} y {}.", ix.columns[0], ix.columns[1]));
                ix.columns.truncate(2);
            }
            let n = valid_name(&ix.name);
            if n != ix.name {
                report.push(Severity::Info, IssueCode::IdentifierRenamed, &source, Some(&ix.name), format!("Nombre de índice válido para DynamoDB: «{n}»."));
                ix.name = n;
            }
            ix.kind = Some("GSI".into());
            kept.push(ix);
        }
        t.indexes = kept;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn parses_type_codes() {
        let p = |s: &str| DynamoDb.parse_type(&parse(s));
        assert_eq!(p("S"), L::Text { unicode: true });
        assert_eq!(p("N"), L::Decimal { precision: None, scale: None });
        assert_eq!(p("B"), L::Blob);
        assert_eq!(p("BOOL"), L::Bool);
        assert_eq!(p("SS"), L::Array { of: Box::new(L::Text { unicode: true }) });
        assert_eq!(p("M"), L::Json { binary: true });
        assert_eq!(p("L"), L::Json { binary: true });
        assert_eq!(p("S | N"), L::Json { binary: true });
        assert_eq!(p("NULL"), L::Text { unicode: true });
        assert_eq!(p(""), L::Json { binary: true });
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| DynamoDb.render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOL");
        assert_eq!(r(L::int(8)), "N");
        assert_eq!(r(L::Decimal { precision: Some(12), scale: Some(2) }), "N");
        assert!(!DynamoDb.render_type(&L::Decimal { precision: None, scale: None }).notes.is_empty());
        assert_eq!(r(L::Float { bytes: 8 }), "N");
        assert!(DynamoDb.render_type(&L::Float { bytes: 8 }).notes.iter().any(|n| n.code == IssueCode::RangeLoss));
        assert_eq!(r(L::Money), "N");
        assert_eq!(r(L::Varchar { len: Some(3), unicode: true }), "S");
        assert_eq!(r(L::Blob), "B");
        assert_eq!(r(L::Bit { len: None }), "S");
        assert_eq!(r(L::Date), "S");
        assert_eq!(r(L::Time { precision: None, tz: false }), "S");
        assert_eq!(r(L::Timestamp { precision: None, tz: true }), "S");
        assert_eq!(r(L::Interval), "S");
        assert_eq!(r(L::Year), "N");
        assert_eq!(r(L::Uuid), "S");
        assert_eq!(r(L::Json { binary: true }), "M");
        assert_eq!(r(L::Xml), "S");
        assert_eq!(r(L::Enum { values: vec![] }), "S");
        assert_eq!(r(L::Set { values: vec![] }), "SS");
        assert_eq!(r(L::Array { of: Box::new(L::Bool) }), "L");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "M");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "S");
        assert_eq!(r(L::Inet), "S");
        assert_eq!(r(L::MacAddr), "S");
        assert_eq!(r(L::RowVersion), "B");
        assert_eq!(r(L::Other { native: "x".into() }), "x");
    }

    #[test]
    fn names() {
        assert_eq!(valid_name("t"), "t__");
        assert_eq!(valid_name("mis ventas"), "mis_ventas");
    }
}
