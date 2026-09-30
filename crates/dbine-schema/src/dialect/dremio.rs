//! Dremio (Iceberg tables in `$scratch`, Nessie, Arctic…) and Apache Drill.
//!
//! Both report SQL-standard names in INFORMATION_SCHEMA (`CHARACTER
//! VARYING`, `BINARY VARYING`, `INTEGER`…). Dremio creates Iceberg tables
//! with plain columns: no keys, NOT NULL, defaults or indexes, timestamps
//! to the millisecond. Drill has no CREATE TABLE with a column list (only
//! CTAS from a query), so it reads as a source only.

use super::influxdb::source_only;
use super::starrocks::{capped_decimal, parse_container, unbounded_decimal, wrap};
use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

const DRILL_WHY: &str = "Drill no tiene CREATE TABLE con columnas (solo CREATE TABLE … AS SELECT sobre archivos).";

pub struct Dremio {
    drill: bool,
}

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static DREMIO: Dremio = Dremio { drill: false };
    static DRILL: Dremio = Dremio { drill: true };
    match driver_id {
        "dremio" => Some(&DREMIO),
        "drill" => Some(&DRILL),
        _ => None,
    }
}

impl Dremio {
    fn name(&self) -> &'static str {
        if self.drill {
            "Drill"
        } else {
            "Dremio"
        }
    }
}

impl Dialect for Dremio {
    fn id(&self) -> &'static str {
        if self.drill {
            "drill"
        } else {
            "dremio"
        }
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        if let Some(c) = parse_container(self, &t.raw) {
            return c;
        }
        let p = |i| t.arg_u32(i);
        let n = t.name.as_str();
        match n {
            "boolean" | "bit" => L::Bool,
            "tinyint" => L::int(1),
            "smallint" => L::int(2),
            "integer" | "int" => L::int(4),
            "bigint" => L::int(8),
            "uint1" => L::Int { bytes: 1, unsigned: true },
            "uint2" => L::Int { bytes: 2, unsigned: true },
            "uint4" => L::Int { bytes: 4, unsigned: true },
            "uint8" => L::Int { bytes: 8, unsigned: true },
            "decimal" | "numeric" | "vardecimal" => L::Decimal { precision: p(0), scale: p(1).or(p(0).map(|_| 0)) },
            "float" | "real" | "float4" => L::Float { bytes: 4 },
            "double" | "double precision" | "float8" => L::Float { bytes: 8 },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: true },
            // 65536 is Drill's "no limit".
            "varchar" | "character varying" => match p(0) {
                Some(n) if n < 65_536 => L::Varchar { len: Some(n), unicode: true },
                _ => L::Text { unicode: true },
            },
            "binary" => L::Binary { len: p(0) },
            "varbinary" | "binary varying" => match p(0) {
                Some(n) if n < 65_536 => L::Varbinary { len: Some(n) },
                _ => L::Blob,
            },
            "date" => L::Date,
            "time" => L::Time { precision: Some(3), tz: false },
            "timestamp" => L::Timestamp { precision: Some(3), tz: false },
            // Iceberg / Arrow zone-normalized timestamps.
            "timestamptz" | "timestamp_ltz" => L::Timestamp { precision: Some(6), tz: true },
            _ if n.starts_with("interval") => L::Interval,
            // Nested types without their members (INFORMATION_SCHEMA says only
            // LIST / STRUCT / MAP): their values come as JSON.
            "list" | "array" | "struct" | "map" | "dict" | "union" | "row" => L::Json { binary: false },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        let engine = self.name();
        let varchar = |why: String| Rendered::exact("VARCHAR").with(Warning, TypeApproximated, why);
        let millis = |p: Option<u8>, r: Rendered| match p {
            Some(p) if p > 3 => r.with(Loss, PrecisionLoss, format!("{engine} guarda milisegundos: se pierden {} decimales de segundo.", p - 3)),
            None => r.with(Warning, PrecisionLoss, format!("{engine} guarda milisegundos: si el origen tiene más precisión, se redondea.")),
            _ => r,
        };
        match t {
            L::Bool => Rendered::exact("BOOLEAN"),
            L::Int { bytes, unsigned } => match L::signed_bytes_for(*bytes, *unsigned) {
                1..=4 => Rendered::exact("INT"),
                8 => Rendered::exact("BIGINT"),
                _ if *bytes == 8 => Rendered::exact("DECIMAL(20, 0)").with(Info, TypeChanged, format!("{engine} no tiene enteros sin signo: BIGINT sin signo como DECIMAL(20, 0).")),
                _ => Rendered::exact("DECIMAL(38, 0)").with(Loss, RangeLoss, "Entero de 16 bytes como DECIMAL(38, 0): los valores de 39 dígitos no entran."),
            },
            L::Decimal { precision: Some(p), scale } => capped_decimal(*p, *scale, 38, engine),
            L::Decimal { precision: None, .. } => unbounded_decimal(engine),
            L::Float { bytes: 4 } => Rendered::exact("FLOAT"),
            L::Float { .. } => Rendered::exact("DOUBLE"),
            L::Money => Rendered::exact("DECIMAL(19, 4)").with(Info, TypeChanged, "Moneda como DECIMAL(19, 4)."),
            L::Char { .. } => Rendered::exact("VARCHAR").with(Info, TypeChanged, format!("{engine} no tiene texto de largo fijo: VARCHAR sin relleno.")),
            L::Varchar { .. } | L::Text { .. } => Rendered::exact("VARCHAR"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("VARBINARY"),
            L::Bit { len } => match len {
                Some(n) if *n <= 63 => Rendered::exact("BIGINT").with(Info, TypeChanged, "Cadena de bits como entero."),
                _ => Rendered::exact("VARBINARY").with(Warning, TypeApproximated, "Cadena de bits larga: se guarda como binario."),
            },
            L::Date => Rendered::exact("DATE"),
            L::Time { precision, tz } => {
                let r = millis(*precision, Rendered::exact("TIME"));
                if *tz {
                    r.with(Loss, TimeZoneLoss, format!("{engine} no guarda la zona horaria de una hora."))
                } else {
                    r
                }
            }
            L::Timestamp { precision, tz } => {
                let r = millis(*precision, Rendered::exact("TIMESTAMP"));
                if *tz {
                    r.with(Warning, TimeZoneLoss, format!("{engine} no guarda la zona horaria: los valores quedan en UTC."))
                } else {
                    r
                }
            }
            L::Interval => varchar(format!("Las tablas de {engine} no tienen intervalos: queda como texto.")),
            L::Year => Rendered::exact("INT").with(Info, TypeChanged, "Año como entero."),
            L::Uuid => Rendered::exact("VARCHAR").with(Info, TypeChanged, "UUID como texto."),
            L::Json { .. } => Rendered::exact("VARCHAR").with(Info, TypeApproximated, "JSON como texto."),
            L::Xml => varchar("XML como texto.".into()),
            L::Enum { values } => varchar(format!("{engine} no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
            L::Set { values } => Rendered::exact("LIST<VARCHAR>").with(Warning, TypeApproximated, format!("Conjunto como lista de texto. Valores: {}.", values.join(", "))),
            L::Array { of } => wrap(self.render_type(of), |i| format!("LIST<{i}>")),
            L::Map { .. } => varchar("Mapa como texto JSON.".into()),
            L::Geometry { .. } => varchar("Dato espacial como texto (WKT).".into()),
            L::Inet => Rendered::exact("VARCHAR").with(Info, TypeApproximated, "Dirección IP como texto."),
            L::MacAddr => Rendered::exact("VARCHAR").with(Info, TypeApproximated, "Dirección MAC como texto."),
            L::RowVersion => Rendered::exact("BIGINT").with(Warning, TypeApproximated, format!("{engine} no tiene versión de fila automática: no se actualiza sola.")),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    /// Iceberg tables in Dremio take no defaults.
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
            max_identifier: 128,
            case: IdentCase::Preserve,
        }
    }

    fn target_refusal(&self, _: &str) -> Option<&'static str> {
        self.drill.then_some(DRILL_WHY)
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        if self.drill {
            source_only(t, report, DRILL_WHY);
            return;
        }
        if let Some(k) = t.primary_key.take() {
            report.push(
                Severity::Warning,
                IssueCode::PrimaryKeyDropped,
                &t.name,
                Some(&k.columns.join(", ")),
                "Las tablas Iceberg de Dremio no tienen clave primaria: la unicidad no se controla.",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn dremio() -> &'static dyn Dialect {
        lookup("dremio").unwrap()
    }
    fn p(d: &dyn Dialect, s: &str) -> L {
        crate::convert::logical_of(d, &parse(s))
    }

    #[test]
    fn parses_information_schema_names() {
        for d in [dremio(), lookup("drill").unwrap()] {
            assert_eq!(p(d, "BOOLEAN"), L::Bool);
            assert_eq!(p(d, "INTEGER"), L::int(4));
            assert_eq!(p(d, "BIGINT"), L::int(8));
            assert_eq!(p(d, "FLOAT"), L::Float { bytes: 4 });
            assert_eq!(p(d, "DOUBLE"), L::Float { bytes: 8 });
            assert_eq!(p(d, "DECIMAL(18,2)"), L::Decimal { precision: Some(18), scale: Some(2) });
            assert_eq!(p(d, "CHARACTER VARYING"), L::Text { unicode: true });
            assert_eq!(p(d, "CHARACTER VARYING(20)"), L::Varchar { len: Some(20), unicode: true });
            assert_eq!(p(d, "BINARY VARYING"), L::Blob);
            assert_eq!(p(d, "DATE"), L::Date);
            assert_eq!(p(d, "TIME"), L::Time { precision: Some(3), tz: false });
            assert_eq!(p(d, "TIMESTAMP"), L::Timestamp { precision: Some(3), tz: false });
            assert_eq!(p(d, "INTERVAL DAY TO SECOND"), L::Interval);
            assert_eq!(p(d, "LIST"), L::Json { binary: false });
            assert_eq!(p(d, "STRUCT"), L::Json { binary: false });
            assert_eq!(p(d, "LIST<INTEGER>"), L::Array { of: Box::new(L::int(4)) });
        }
        let drill = lookup("drill").unwrap();
        assert_eq!(p(drill, "FLOAT8"), L::Float { bytes: 8 });
        assert_eq!(p(drill, "VARDECIMAL"), L::Decimal { precision: None, scale: None });
        assert_eq!(p(drill, "BIT"), L::Bool);
        assert!(matches!(p(drill, "ANY"), L::Other { .. }));
    }

    #[test]
    fn renders_every_variant() {
        let r = |t: L| dremio().render_type(&t).native;
        assert_eq!(r(L::Bool), "BOOLEAN");
        assert_eq!(r(L::int(2)), "INT");
        assert_eq!(r(L::Int { bytes: 4, unsigned: true }), "BIGINT");
        assert_eq!(r(L::Int { bytes: 8, unsigned: true }), "DECIMAL(20, 0)");
        assert_eq!(r(L::int(16)), "DECIMAL(38, 0)");
        assert_eq!(r(L::Decimal { precision: Some(10), scale: Some(2) }), "DECIMAL(10, 2)");
        assert_eq!(r(L::Decimal { precision: None, scale: None }), "DECIMAL(38, 10)");
        assert_eq!(r(L::Float { bytes: 4 }), "FLOAT");
        assert_eq!(r(L::Float { bytes: 8 }), "DOUBLE");
        assert_eq!(r(L::Money), "DECIMAL(19, 4)");
        assert_eq!(r(L::Char { len: Some(3), unicode: true }), "VARCHAR");
        assert_eq!(r(L::Varchar { len: Some(3), unicode: true }), "VARCHAR");
        assert_eq!(r(L::Text { unicode: true }), "VARCHAR");
        assert_eq!(r(L::Binary { len: Some(3) }), "VARBINARY");
        assert_eq!(r(L::Blob), "VARBINARY");
        assert_eq!(r(L::Bit { len: Some(3) }), "BIGINT");
        assert_eq!(r(L::Date), "DATE");
        assert_eq!(r(L::Time { precision: Some(6), tz: false }), "TIME");
        assert_eq!(r(L::Timestamp { precision: Some(6), tz: true }), "TIMESTAMP");
        assert_eq!(r(L::Interval), "VARCHAR");
        assert_eq!(r(L::Year), "INT");
        assert_eq!(r(L::Uuid), "VARCHAR");
        assert_eq!(r(L::Json { binary: true }), "VARCHAR");
        assert_eq!(r(L::Xml), "VARCHAR");
        assert_eq!(r(L::Enum { values: vec![] }), "VARCHAR");
        assert_eq!(r(L::Set { values: vec![] }), "LIST<VARCHAR>");
        assert_eq!(r(L::Array { of: Box::new(L::int(8)) }), "LIST<BIGINT>");
        assert_eq!(r(L::Map { key: Box::new(L::Bool), value: Box::new(L::Bool) }), "VARCHAR");
        assert_eq!(r(L::Geometry { kind: None, srid: None, geography: false }), "VARCHAR");
        assert_eq!(r(L::Inet), "VARCHAR");
        assert_eq!(r(L::MacAddr), "VARCHAR");
        assert_eq!(r(L::RowVersion), "BIGINT");
        let ts = dremio().render_type(&L::Timestamp { precision: Some(6), tz: false });
        assert!(ts.notes.iter().any(|n| n.code == IssueCode::PrecisionLoss && n.severity == Severity::Loss));
    }

    #[test]
    fn drill_is_source_only() {
        let mut t = TableSchema { name: "t".into(), ..Default::default() };
        let mut rep = Report::default();
        lookup("drill").unwrap().finalize(&mut t, &mut rep);
        assert_eq!(rep.issues[0].severity, Severity::Dropped);
    }
}
