//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`])
//! in the tree model (the REST API's), IoTDB 1.x and 2.x alike:
//!
//! - the TTL of the whole database: `SET TTL TO root.x <ms>` (the server
//!   extends it to `root.x.**`) or `UNSET TTL TO root.x`;
//! - the region group counts, the only properties `ALTER DATABASE … WITH`
//!   changes at run time (`SCHEMA_REGION_GROUP_NUM`, `DATA_REGION_GROUP_NUM`,
//!   in one statement);
//! - facts from `SHOW DATABASES DETAILS` (replication factors, time
//!   partitions, region groups), the device and series counts.
//!
//! IoTDB 1.x reports the TTL in `SHOW DATABASES DETAILS`; 2.x only in
//! `SHOW TTL ON`.

use crate::create_db::{count, millis};
use crate::{database_path, IotDbSession, Table};
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use serde_json::Value as J;
use std::collections::BTreeMap;

const REGIONS: &str = "Regiones";

/// The statements for `changes`: the TTL first, then the region groups.
pub(crate) fn alter(database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let path = database_path(database);
    let mut out = Vec::new();
    let mut with = Vec::new();
    for (key, value) in changes {
        let value = value.trim();
        match key.as_str() {
            "ttl" => out.push(if value.is_empty() {
                format!("UNSET TTL TO {path}")
            } else if value.eq_ignore_ascii_case("inf") {
                format!("SET TTL TO {path} INF")
            } else {
                format!("SET TTL TO {path} {}", millis(value, "TTL")?)
            }),
            "schema_region_group_num" => with.push(format!("SCHEMA_REGION_GROUP_NUM={}", count(value, "grupos de regiones de esquema")?)),
            "data_region_group_num" => with.push(format!("DATA_REGION_GROUP_NUM={}", count(value, "grupos de regiones de datos")?)),
            k => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
        }
    }
    // BTreeMap order puts the data groups before the schema ones; the
    // statement lists them as the docs do.
    with.sort_by_key(|w| !w.starts_with("SCHEMA"));
    if !with.is_empty() {
        out.push(format!("ALTER DATABASE {path} WITH {}", with.join(", ")));
    }
    Ok(out)
}

pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(database, changes)?.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n"))
}

/// Milliseconds in the largest unit that divides them (`7d`, `90m`).
fn human_ms(ms: u64) -> String {
    for (unit, size) in [("w", 604_800_000), ("d", 86_400_000), ("h", 3_600_000), ("m", 60_000), ("s", 1_000)] {
        if ms >= size && ms.is_multiple_of(size) {
            return format!("{}{unit}", ms / size);
        }
    }
    ms.to_string()
}

fn text(v: Option<&J>) -> String {
    match v {
        Some(J::String(s)) => s.clone(),
        Some(J::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

/// The first row as column → value.
fn first_row(t: &Table) -> BTreeMap<String, String> {
    t.rows.first().map(|r| t.columns.iter().map(|c| c.name.clone()).zip(r.iter().map(|v| text(Some(v)))).collect()).unwrap_or_default()
}

/// A TTL as the field shows it: `INF` when a rule says the data never
/// expire, empty when there is no rule.
fn ttl_value(v: &str) -> String {
    let v = v.trim();
    if v.eq_ignore_ascii_case("inf") {
        return "INF".into();
    }
    match v.parse::<u64>() {
        Ok(ms) if ms >= i64::MAX as u64 => "INF".into(),
        Ok(ms) if ms > 0 => human_ms(ms),
        _ => String::new(),
    }
}

impl IotDbSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let path = database_path(database);
        let d = first_row(&self.query(&format!("SHOW DATABASES DETAILS {path}"), 10).await?);
        if d.is_empty() {
            return Err(Error::Query(format!("no existe la base {path}")));
        }
        let mut values = BTreeMap::new();
        // 1.x: a column; 2.x: the rule on `root.x.**` (or `root.x`).
        let ttl = match d.get("TTL") {
            Some(t) => ttl_value(t),
            None => {
                let t = self.query(&format!("SHOW TTL ON {path}.**"), 100).await?;
                let rule = t.rows.iter().find(|r| text(r.first()) == format!("{path}.**")).or_else(|| t.rows.iter().find(|r| text(r.first()) == path));
                rule.map(|r| ttl_value(&text(r.get(1)))).unwrap_or_default()
            }
        };
        values.insert("ttl".to_string(), ttl);
        for (key, col) in [("schema_region_group_num", "MinSchemaRegionGroupNum"), ("data_region_group_num", "MinDataRegionGroupNum")] {
            if let Some(v) = d.get(col).filter(|v| !v.is_empty()) {
                values.insert(key.to_string(), v.clone());
            }
        }

        let g = |group: &str, label: &str, value: String| PropertyInfo { group: group.into(), label: label.into(), value };
        let col = |k: &str| d.get(k).cloned().unwrap_or_default();
        let mut info = vec![
            g("", "Réplicas del esquema (SchemaReplicationFactor)", col("SchemaReplicationFactor")),
            g("", "Réplicas de los datos (DataReplicationFactor)", col("DataReplicationFactor")),
            g("", "Intervalo de partición por tiempo (TimePartitionInterval)", col("TimePartitionInterval").parse::<u64>().map(human_ms).unwrap_or_default()),
        ];
        if let Some(o) = d.get("TimePartitionOrigin") {
            info.push(g("", "Origen de las particiones por tiempo (TimePartitionOrigin)", o.clone()));
        }
        for (label, q) in [("Dispositivos", format!("COUNT DEVICES {path}.**")), ("Series temporales", format!("COUNT TIMESERIES {path}.**"))] {
            if let Ok(t) = self.query(&q, 10).await {
                info.push(g("", label, t.rows.first().map(|r| text(r.last())).unwrap_or_default()));
            }
        }
        for (label, cur, max) in [
            ("Grupos de regiones de esquema", "SchemaRegionGroupNum", "MaxSchemaRegionGroupNum"),
            ("Grupos de regiones de datos", "DataRegionGroupNum", "MaxDataRegionGroupNum"),
        ] {
            info.push(g(REGIONS, &format!("{label}: actuales"), col(cur)));
            info.push(g(REGIONS, &format!("{label}: máximo"), col(max)));
        }

        let fields = vec![
            Field::new("ttl", "Vida de los datos (TTL)", FieldKind::Text)
                .placeholder("30d o 2592000000")
                .help("Vacía: sin regla propia (vale la del cluster). INF: no vencen. Con unidad (ms, s, m, h, d, w) o en milisegundos."),
            Field::new("schema_region_group_num", "Grupos de regiones de esquema (SCHEMA_REGION_GROUP_NUM)", FieldKind::Number)
                .help("Con la política CUSTOM del cluster es la cantidad exacta; con AUTO, el mínimo.")
                .group(REGIONS),
            Field::new("data_region_group_num", "Grupos de regiones de datos (DATA_REGION_GROUP_NUM)", FieldKind::Number)
                .help("Con la política CUSTOM del cluster es la cantidad exacta; con AUTO, el mínimo.")
                .group(REGIONS),
        ];
        let warnings = [(
            "ttl".to_string(),
            "Si la vida nueva es más corta, los datos más viejos que ella dejan de verse y se borran en la próxima compactación.".to_string(),
        )]
        .into();
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden modificar las propiedades de una base.".into()));
        }
        let statements = alter(database, changes)?;
        for (i, sql) in statements.iter().enumerate() {
            if let Err(e) = self.non_query(sql).await {
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {sql}\n{e}", statements.len())) });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn ttl_then_region_groups() {
        assert_eq!(
            script("planta", &c(&[("ttl", "7d"), ("data_region_group_num", "3"), ("schema_region_group_num", "2")])).unwrap(),
            "SET TTL TO root.planta 604800000;\nALTER DATABASE root.planta WITH SCHEMA_REGION_GROUP_NUM=2, DATA_REGION_GROUP_NUM=3;"
        );
        assert_eq!(script("root.a.b", &c(&[("ttl", "")])).unwrap(), "UNSET TTL TO root.a.b;");
        assert_eq!(script("p", &c(&[("ttl", "inf")])).unwrap(), "SET TTL TO root.p INF;");
        assert_eq!(script("p", &c(&[("data_region_group_num", "4")])).unwrap(), "ALTER DATABASE root.p WITH DATA_REGION_GROUP_NUM=4;");
        assert_eq!(script("p", &c(&[])).unwrap(), "");
    }

    #[test]
    fn values_are_checked() {
        for bad in [("ttl", "0"), ("ttl", "1y"), ("ttl", "1h; DELETE DATABASE root.p"), ("data_region_group_num", "0"), ("schema_region_group_num", "x"), ("ttl_x", "1")] {
            assert!(script("p", &c(&[bad])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ttl_as_shown() {
        assert_eq!(ttl_value("604800000"), "1w");
        assert_eq!(ttl_value("7200000"), "2h");
        assert_eq!(ttl_value("1500"), "1500");
        assert_eq!(ttl_value("9223372036854775807"), "INF");
        assert_eq!(ttl_value("INF"), "INF");
        assert_eq!(ttl_value(""), "");
    }
}
