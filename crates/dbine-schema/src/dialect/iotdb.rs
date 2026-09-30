//! Apache IoTDB and TimechoDB, tree model: a table becomes a device
//! (`root.<database>.<name>`) and each column a time series (measurement)
//! under it. The time is implicit (`Time`, one value per instant: writing
//! an existing time overwrites it); there are no keys, NOT NULL, defaults,
//! indexes nor comments. Types: BOOLEAN, INT32, INT64, FLOAT, DOUBLE,
//! TEXT / STRING, BLOB, DATE and TIMESTAMP (the last four since 1.3.3).

use super::greptimedb::time_column;
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{ColumnDef, KeyDef, TableSchema};

pub struct IotDb;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: IotDb = IotDb;
    matches!(driver_id, "iotdb" | "timechodb").then_some(&D as &dyn Dialect)
}

/// DATE, TIMESTAMP, BLOB and STRING series exist since IoTDB 1.3.3; text
/// goes as TEXT, which every version has.
const NEW_TYPES: &str = "Series de tipo DATE, TIMESTAMP o BLOB: requieren IoTDB 1.3.3 o posterior.";

/// The device's time column, as the driver reports and expects it.
pub const TIME: &str = "Time";

fn is_time_name(n: &str) -> bool {
    n.eq_ignore_ascii_case("time") || n.eq_ignore_ascii_case("timestamp")
}

impl Dialect for IotDb {
    fn id(&self) -> &'static str {
        "iotdb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        match t.name.as_str() {
            "boolean" => L::Bool,
            "int32" => L::int(4),
            "int64" => L::int(8),
            "float" => L::Float { bytes: 4 },
            "double" => L::Float { bytes: 8 },
            "text" | "string" => L::Text { unicode: true },
            "blob" => L::Blob,
            "date" => L::Date,
            // Epoch milliseconds by default (the server's timestamp_precision).
            "timestamp" => L::Timestamp { precision: Some(3), tz: true },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let string = |why: String| Rendered::exact("TEXT").with(Warning, TypeApproximated, why);
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => match L::signed_bytes_for(*bytes, *unsigned) {
                1..=4 => Rendered::exact("INT32"),
                8 => Rendered::exact("INT64"),
                _ => string("IoTDB no tiene enteros de más de 8 bytes (ni sin signo de 8): queda como texto.".into()),
            },
            L::Decimal { .. } | L::Money => {
                Rendered::exact("DOUBLE").with(Loss, PrecisionLoss, "IoTDB no tiene decimales exactos: se guardan como DOUBLE (unos 15 dígitos).")
            }
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } => Rendered::exact("TEXT"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("BLOB").with(Info, TypeChanged, NEW_TYPES),
            L::Bit { len } => match len {
                Some(n) if *n <= 63 => Rendered::exact("INT64").with(Info, TypeChanged, "Cadena de bits como entero."),
                _ => Rendered::exact("BLOB").with(Warning, TypeApproximated, "Cadena de bits larga: se guarda como binario."),
            },
            L::Date => Rendered::exact("DATE").with(Info, TypeChanged, NEW_TYPES),
            L::Time { tz, .. } => {
                let r = string("IoTDB no tiene horas: queda como texto HH:MM:SS.".into());
                if *tz {
                    r.with(Loss, TimeZoneLoss, "Se pierde la zona horaria de la hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let mut r = Rendered::exact("TIMESTAMP").with(Info, TypeChanged, NEW_TYPES);
                if precision.is_none_or(|p| p > 3) {
                    r = r.with(Warning, PrecisionLoss, "IoTDB guarda milisegundos salvo que el servidor use timestamp_precision us o ns.");
                }
                if !tz {
                    r = r.with(Info, TimeZoneLoss, "IoTDB guarda instantes: los valores sin zona se interpretan en la zona del cliente.");
                }
                r
            }
            L::Interval => string("IoTDB no tiene intervalos: queda como texto.".into()),
            L::Year => Rendered::exact("INT32").with(Info, TypeChanged, "Año como entero."),
            L::Uuid => Rendered::exact("TEXT").with(Info, TypeChanged, "UUID como texto."),
            L::Json { .. } => Rendered::exact("TEXT").with(Info, TypeApproximated, "JSON como texto."),
            L::Xml => string("XML como texto.".into()),
            L::Enum { values } => string(format!("IoTDB no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => string(format!("Conjunto como texto. Valores: {}.", values.join(", "))),
            L::Array { .. } | L::Map { .. } => string("IoTDB no tiene arreglos ni mapas: se guarda como texto JSON.".into()),
            L::Geometry { .. } => string("Dato espacial como texto (WKT).".into()),
            L::Inet => Rendered::exact("TEXT").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("TEXT").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("INT64").with(Warning, TypeApproximated, "IoTDB no tiene versión de fila automática: no se actualiza sola."),
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
            comments: false,
            max_identifier: 255,
            case: IdentCase::Preserve,
        }
    }

    /// The device's `Time`: a column already named so, else the key's
    /// timestamp (or the first one), renamed; else a new one. Converted
    /// tables become aligned devices (one timestamp per row, like a table).
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        use IssueCode::*;
        use Severity::*;
        let table = t.name.clone();
        let time_at = match t.columns.iter().position(|c| is_time_name(&c.name)) {
            Some(i) => i,
            None => match time_column(t, |s| s.name == "timestamp") {
                Some(i) => {
                    let old = std::mem::replace(&mut t.columns[i].name, TIME.to_string());
                    report.push(Warning, IdentifierRenamed, &table, Some(&old), format!(
                        "«{old}» pasa a ser el tiempo del dispositivo ({TIME}): una fila por instante, repetir uno sobrescribe la fila."
                    ));
                    i
                }
                None => {
                    t.columns.insert(0, ColumnDef { name: TIME.into(), data_type: "TIMESTAMP".into(), nullable: false, ..Default::default() });
                    report.push(Warning, PrimaryKeyAdded, &table, Some(TIME), format!(
                        "Cada dato de IoTDB lleva su tiempo: se agrega «{TIME}». Al copiar hay que darle un valor distinto por fila (repetir uno sobrescribe la fila)."
                    ));
                    0
                }
            },
        };
        let c = t.columns.remove(time_at);
        let time = c.name.clone();
        t.columns.insert(0, ColumnDef { data_type: "TIMESTAMP".into(), nullable: false, ..c });
        if let Some(k) = t.primary_key.take().filter(|k| k.columns != [time.clone()]) {
            report.push(Warning, PrimaryKeyDropped, &table, Some(&k.columns.join(", ")), format!(
                "IoTDB no tiene claves primarias: cada fila del dispositivo se identifica por «{time}»."
            ));
        }
        t.primary_key = Some(KeyDef { name: None, columns: vec![time] });
        if !t.options.contains_key("aligned") {
            t.options.insert("aligned".into(), "true".into());
            report.push(Info, OptionAdded, &table, Some("aligned"), "Dispositivo alineado: sus series comparten el tiempo, como las columnas de una fila.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn p(s: &str) -> L {
        crate::convert::logical_of(&IotDb, &parse(s))
    }

    #[test]
    fn parses_series_types() {
        assert_eq!(p("BOOLEAN"), L::Bool);
        assert_eq!(p("INT32"), L::int(4));
        assert_eq!(p("INT64"), L::int(8));
        assert_eq!(p("FLOAT"), L::Float { bytes: 4 });
        assert_eq!(p("DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(p("TEXT"), L::Text { unicode: true });
        assert_eq!(p("STRING"), L::Text { unicode: true });
        assert_eq!(p("BLOB"), L::Blob);
        assert_eq!(p("DATE"), L::Date);
        assert_eq!(p("TIMESTAMP"), L::Timestamp { precision: Some(3), tz: true });
        assert!(matches!(p("VECTOR"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| IotDb.render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(2)), "INT32");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "INT64");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "TEXT");
        assert_eq!(r(L::Decimal { precision: Some(10), scale: Some(2) }), "DOUBLE");
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Float { bytes: 8 }), "DOUBLE");
        assert_eq!(r(L::Money), "DOUBLE");
        assert_eq!(r(L::Char { len: Some(2), unicode: true }), "TEXT");
        assert_eq!(r(L::Varchar { len: Some(2), unicode: true }), "TEXT");
        assert_eq!(r(L::Text { unicode: true }), "TEXT");
        assert_eq!(r(L::Binary { len: Some(2) }), "BLOB");
        assert_eq!(r(L::Varbinary { len: None }), "BLOB");
        assert_eq!(r(L::Blob), "BLOB");
        assert_eq!(r(L::Bit { len: Some(2) }), "INT64");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: None, tz: false }), "TEXT");
        assert_eq!(r(L::Timestamp { precision: Some(3), tz: true }), "TIMESTAMP");
        assert_eq!(r(L::Interval), "TEXT");
        assert_eq!(r(L::Year), "INT32");
        assert_eq!(r(L::Uuid), "TEXT");
        assert_eq!(r(L::Json { binary: true }), "TEXT");
        assert_eq!(r(L::Xml), "TEXT");
        assert_eq!(r(L::Enum { values: vec![] }), "TEXT");
        assert_eq!(r(L::Set { values: vec![] }), "TEXT");
        assert_eq!(r(L::Array { of: Box::new(L::Bool) }), "TEXT");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "TEXT");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "TEXT");
        assert_eq!(r(L::Inet), "TEXT");
        assert_eq!(r(L::MacAddr), "TEXT");
        assert_eq!(r(L::RowVersion), "INT64");
    }

    #[test]
    fn finalize_finds_or_adds_the_time() {
        let col = |n: &str, ty: &str| ColumnDef { name: n.into(), data_type: ty.into(), ..Default::default() };
        let mut t = TableSchema { name: "d".into(), columns: vec![col("v", "DOUBLE"), col("alta", "TIMESTAMP")], ..Default::default() };
        let mut rep = Report::default();
        IotDb.finalize(&mut t, &mut rep);
        assert_eq!(t.columns[0].name, TIME);
        assert_eq!(t.options.get("aligned").map(String::as_str), Some("true"));
        assert!(rep.issues.iter().any(|i| i.code == IssueCode::IdentifierRenamed));

        let mut t = TableSchema { name: "d".into(), columns: vec![col("v", "DOUBLE")], ..Default::default() };
        let mut rep = Report::default();
        IotDb.finalize(&mut t, &mut rep);
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.primary_key.as_ref().unwrap().columns, vec![TIME.to_string()]);
    }
}
