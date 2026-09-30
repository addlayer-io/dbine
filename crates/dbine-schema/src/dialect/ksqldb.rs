//! ksqlDB: streams and tables over Kafka topics. A table needs a PRIMARY
//! KEY; a stream may have KEY columns. No NOT NULL, defaults, indexes,
//! foreign keys nor comments. Types: BOOLEAN, INT, BIGINT, DOUBLE,
//! DECIMAL, STRING (VARCHAR), BYTES, DATE, TIME and TIMESTAMP (millisecond
//! precision), ARRAY, MAP and STRUCT.

use super::starrocks::{capped_decimal, merge, parse_container, unbounded_decimal, wrap};
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::{self, TypeSpec};
use dbine_driver::{kinds, TableSchema};

pub struct KsqlDb;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: KsqlDb = KsqlDb;
    (driver_id == "ksqldb").then_some(&D as &dyn Dialect)
}

impl Dialect for KsqlDb {
    fn id(&self) -> &'static str {
        "ksqldb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        if let Some(c) = parse_container(self, &t.raw) {
            return c;
        }
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "boolean" => L::Bool,
            "int" | "integer" => L::int(4),
            "bigint" => L::int(8),
            "double" => L::Float { bytes: 8 },
            "decimal" => L::Decimal { precision: p(0), scale: p(1).or(p(0).map(|_| 0)) },
            "string" | "varchar" => L::Text { unicode: true },
            "bytes" => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: Some(3), tz: false },
            "timestamp" => L::Timestamp { precision: Some(3), tz: false },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let string = |why: String| Rendered::exact("STRING").with(Warning, TypeApproximated, why);
        let millis = |p: Option<u8>, r: Rendered| match p {
            Some(p) if p > 3 => r.with(Loss, PrecisionLoss, format!("ksqlDB guarda milisegundos: se pierden {} decimales de segundo.", p - 3)),
            None => r.with(Warning, PrecisionLoss, "ksqlDB guarda milisegundos: si el origen tiene más precisión, se redondea."),
            _ => r,
        };
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => match L::signed_bytes_for(*bytes, *unsigned) {
                1..=4 => Rendered::exact("INT"),
                8 => Rendered::exact("BIGINT"),
                _ if *bytes == 8 => Rendered::exact("DECIMAL(20, 0)").with(Info, TypeChanged, "ksqlDB no tiene enteros sin signo: BIGINT sin signo como DECIMAL(20, 0)."),
                _ => Rendered::exact("DECIMAL(38, 0)").with(Loss, RangeLoss, "Entero de 16 bytes como DECIMAL(38, 0): los valores de 39 dígitos no entran."),
            },
            L::Decimal { precision: Some(p), scale } => capped_decimal(*p, *scale, 38, "ksqlDB"),
            L::Decimal { precision: None, .. } => unbounded_decimal("ksqlDB"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(Info, TypeChanged, "Moneda como DECIMAL(19, 4)."),
            L::Char { .. } => Rendered::exact("STRING").with(Info, TypeChanged, "ksqlDB no tiene texto de largo fijo: STRING sin relleno."),
            L::Varchar { .. } | L::Text { .. } => Rendered::exact("STRING"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("BYTES"),
            L::Bit { len } => match len {
                Some(n) if *n <= 63 => Rendered::exact("BIGINT").with(Info, TypeChanged, "Cadena de bits como entero."),
                _ => Rendered::exact("BYTES").with(Warning, TypeApproximated, "Cadena de bits larga: se guarda como binario."),
            },
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => {
                let r = millis(*precision, Rendered::exact("TIME"));
                if *tz {
                    r.with(Loss, TimeZoneLoss, "ksqlDB no guarda la zona horaria de una hora.")
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let r = millis(*precision, Rendered::exact("TIMESTAMP"));
                if *tz {
                    r.with(Info, TimeZoneLoss, "TIMESTAMP de ksqlDB es un instante en UTC: se pierde la zona de origen.")
                } else {
                    r
                }
            }
            L::Interval => string("ksqlDB no tiene intervalos: queda como texto.".into()),
            L::Year => Rendered::exact("INT").with(Info, TypeChanged, "Año como entero."),
            L::Uuid => Rendered::exact("STRING").with(Info, TypeChanged, "UUID como texto."),
            L::Json { .. } => Rendered::exact("STRING").with(Info, TypeApproximated, "JSON como texto."),
            L::Xml => string("XML como texto.".into()),
            L::Enum { values } => string(format!("ksqlDB no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => Rendered::exact("ARRAY<STRING>").with(Warning, TypeApproximated, format!("Conjunto como arreglo de texto. Valores: {}.", values.join(", "))),
            L::Array { of } => wrap(self.render_type(of), |i| format!("ARRAY<{i}>")),
            L::Map { key, value } => {
                let k = self.render_type(key);
                let k = if k.native == "STRING" { k } else { string("ksqlDB solo admite claves de texto en los mapas.".into()) };
                merge(k, self.render_type(value), |k, v| format!("MAP<{k}, {v}>"))
            }
            L::Geometry { .. } => string("Dato espacial como texto (WKT).".into()),
            L::Inet => Rendered::exact("STRING").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("STRING").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("BIGINT").with(Warning, TypeApproximated, "ksqlDB no tiene versión de fila automática: no se actualiza sola."),
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
            // Unquoted names are upper-cased.
            case: IdentCase::Upper,
        }
    }

    /// TABLE with the primary key, STREAM without one; the topic gets one
    /// partition (the designer's default); keys that the KAFKA format can't
    /// serialize (several columns, non-primitive types) use JSON.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        use IssueCode::*;
        use Severity::*;
        let table = t.name.clone();
        let pk: Vec<String> = t.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default();
        if !t.options.contains_key("object") {
            let (object, kind, why) = if pk.is_empty() {
                ("STREAM", kinds::STREAM, "Sin clave primaria: se crea un STREAM (eventos que solo se agregan).".to_string())
            } else {
                ("TABLE", kinds::TABLE, "Con clave primaria: se crea una TABLE (cada clave guarda su último valor; un INSERT con la misma clave reemplaza la fila).".to_string())
            };
            t.options.insert("object".into(), object.into());
            t.kind = kind.into();
            report.push(Info, OptionAdded, &table, Some("object"), why);
        }
        if !t.options.contains_key("PARTITIONS") {
            t.options.insert("PARTITIONS".into(), "1".into());
            report.push(Info, OptionAdded, &table, Some("PARTITIONS"), "Topic con una partición (lo que propone el diseñador), si hay que crearlo.");
        }
        let primitive = |c: &String| {
            t.columns.iter().find(|x| &x.name == c).is_some_and(|x| matches!(parse::parse(&x.data_type).name.as_str(), "int" | "bigint" | "double" | "string" | "bytes"))
        };
        if !pk.is_empty() && !t.options.contains_key("KEY_FORMAT") && (pk.len() > 1 || !pk.iter().all(primitive)) {
            t.options.insert("KEY_FORMAT".into(), "JSON".into());
            report.push(Info, OptionAdded, &table, Some("KEY_FORMAT"), "Clave en formato JSON: el formato KAFKA solo admite una columna de tipo simple.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use dbine_driver::{ColumnDef, KeyDef};

    fn p(s: &str) -> L {
        crate::convert::logical_of(&KsqlDb, &parse(s))
    }

    #[test]
    fn parses_described_types() {
        assert_eq!(p("BOOLEAN"), L::Bool);
        assert_eq!(p("INTEGER"), L::int(4));
        assert_eq!(p("INT"), L::int(4));
        assert_eq!(p("BIGINT"), L::int(8));
        assert_eq!(p("DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(p("DECIMAL(10, 2)"), L::Decimal { precision: Some(10), scale: Some(2) });
        assert_eq!(p("STRING"), L::Text { unicode: true });
        assert_eq!(p("VARCHAR"), L::Text { unicode: true });
        assert_eq!(p("BYTES"), L::Blob);
        assert_eq!(p("DATE"), L::Date);
        assert_eq!(p("TIME"), L::Time { precision: Some(3), tz: false });
        assert_eq!(p("TIMESTAMP"), L::Timestamp { precision: Some(3), tz: false });
        assert_eq!(p("ARRAY<STRING>"), L::Array { of: Box::new(L::Text { unicode: true }) });
        assert_eq!(p("MAP<STRING, INTEGER>"), L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::int(4)) });
        assert_eq!(p("STRUCT<A INTEGER, B STRING>"), L::Json { binary: false });
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| KsqlDb.render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(2)), "INT");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "BIGINT");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "DECIMAL(20, 0)");
        assert_eq!(r(L::int(16)), "DECIMAL(38, 0)");
        assert_eq!(r(L::Decimal { precision: Some(10), scale: Some(2) }), "DECIMAL(10, 2)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "DECIMAL(38, 10)");
        assert_eq!(r(L::Float { bytes: 4 }), "DOUBLE");
        assert_eq!(r(L::Money), "DECIMAL(19, 4)");
        assert_eq!(r(L::Char { len: Some(1), unicode: true }), "STRING");
        assert_eq!(r(L::Varchar { len: Some(1), unicode: true }), "STRING");
        assert_eq!(r(L::Text { unicode: true }), "STRING");
        assert_eq!(r(L::Binary { len: Some(1) }), "BYTES");
        assert_eq!(r(L::Blob), "BYTES");
        assert_eq!(r(L::Bit { len: Some(1) }), "BIGINT");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: Some(3), tz: false }), "TIME");
        assert_eq!(r(L::Timestamp { precision: Some(3), tz: false }), "TIMESTAMP");
        assert_eq!(r(L::Interval), "STRING");
        assert_eq!(r(L::Year), "INT");
        assert_eq!(r(L::Uuid), "STRING");
        assert_eq!(r(L::Json { binary: true }), "STRING");
        assert_eq!(r(L::Xml), "STRING");
        assert_eq!(r(L::Enum { values: vec![] }), "STRING");
        assert_eq!(r(L::Set { values: vec![] }), "ARRAY<STRING>");
        assert_eq!(r(L::Array { of: Box::new(L::int(8)) }), "ARRAY<BIGINT>");
        assert_eq!(r(L::Map { key: Box::new(L::int(4)), value: Box::new(L::int(8)) }), "MAP<STRING, BIGINT>");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "STRING");
        assert_eq!(r(L::Inet), "STRING");
        assert_eq!(r(L::MacAddr), "STRING");
        assert_eq!(r(L::RowVersion), "BIGINT");
    }

    #[test]
    fn finalize_picks_table_or_stream() {
        let col = |n: &str, ty: &str| ColumnDef { name: n.into(), data_type: ty.into(), ..Default::default() };
        let mut t = TableSchema {
            kind: "table".into(),
            name: "T".into(),
            columns: vec![col("A", "INT"), col("B", "DECIMAL(10, 2)")],
            primary_key: Some(KeyDef { name: None, columns: vec!["A".into(), "B".into()] }),
            ..Default::default()
        };
        let mut rep = Report::default();
        KsqlDb.finalize(&mut t, &mut rep);
        assert_eq!(t.options.get("object").map(String::as_str), Some("TABLE"));
        assert_eq!(t.options.get("KEY_FORMAT").map(String::as_str), Some("JSON"));
        let mut s = TableSchema { kind: "table".into(), name: "S".into(), columns: vec![col("A", "INT")], ..Default::default() };
        KsqlDb.finalize(&mut s, &mut rep);
        assert_eq!((s.options.get("object").map(String::as_str), s.kind.as_str()), (Some("STREAM"), kinds::STREAM));
    }
}
