//! "Nueva base de datos" with options
//! ([`dbine_driver::Driver::create_database_fields`]): the properties of
//! `CREATE DATABASE root.x WITH …` in the tree model, namely TTL, time
//! partition interval, region group counts and, on IoTDB 1.x, the
//! replication factors (IoTDB 2 takes them only from the cluster's
//! configuration and refuses them here).
//!
//! Durations take a unit (`7d`, `12h`) or a number of milliseconds; every
//! value is checked before it reaches the statement.

use crate::{database_path, IotDbSession};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use serde_json::Value as J;
use std::collections::BTreeMap;

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new("ttl", "Vida de los datos (TTL)", FieldKind::Text)
            .placeholder("30d o 2592000000")
            .help("Vacía: los datos no vencen. Con unidad (ms, s, m, h, d, w) o en milisegundos."),
        Field::new("time_partition_interval", "Intervalo de partición por tiempo (TIME_PARTITION_INTERVAL)", FieldKind::Text)
            .placeholder("7d")
            .help("Vacío: el del cluster."),
        Field::new("schema_region_group_num", "Grupos de regiones de esquema (SCHEMA_REGION_GROUP_NUM)", FieldKind::Number)
            .help("Vacío: lo decide el cluster."),
        Field::new("data_region_group_num", "Grupos de regiones de datos (DATA_REGION_GROUP_NUM)", FieldKind::Number)
            .help("Vacío: lo decide el cluster."),
        Field::new("schema_replication_factor", "Réplicas del esquema (SCHEMA_REPLICATION_FACTOR)", FieldKind::Number)
            .help("Vacío: el del cluster. Solo IoTDB 1.x; IoTDB 2 lo toma de la configuración del cluster."),
        Field::new("data_replication_factor", "Réplicas de los datos (DATA_REPLICATION_FACTOR)", FieldKind::Number)
            .help("Vacío: el del cluster. Solo IoTDB 1.x; IoTDB 2 lo toma de la configuración del cluster."),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// `7d`, `12h`, `500ms` or a bare number of milliseconds, in milliseconds.
fn millis(v: &str, what: &str) -> Result<u64> {
    let s = v.to_ascii_lowercase();
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    let unit = match &s[digits..] {
        "" | "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => 0,
    };
    s[..digits]
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0 && unit > 0)
        .and_then(|n| n.checked_mul(unit))
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or_else(|| Error::Query(format!("{what}: «{v}» no es una duración válida (por ejemplo 7d, 12h o 3600000)")))
}

fn count(v: &str, what: &str) -> Result<u32> {
    v.parse::<u32>().ok().filter(|n| (1..=10_000).contains(n)).ok_or_else(|| Error::Query(format!("{what}: «{v}» no es un valor válido")))
}

/// The statement that creates `name`.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let mut with = Vec::new();
    if let Some(v) = opt(o, "ttl") {
        with.push(format!("TTL={}", millis(v, "TTL")?));
    }
    for (key, prop, what) in [
        ("schema_replication_factor", "SCHEMA_REPLICATION_FACTOR", "réplicas del esquema"),
        ("data_replication_factor", "DATA_REPLICATION_FACTOR", "réplicas de los datos"),
    ] {
        if let Some(v) = opt(o, key) {
            with.push(format!("{prop}={}", count(v, what)?));
        }
    }
    if let Some(v) = opt(o, "time_partition_interval") {
        with.push(format!("TIME_PARTITION_INTERVAL={}", millis(v, "intervalo de partición")?));
    }
    for (key, prop, what) in [
        ("schema_region_group_num", "SCHEMA_REGION_GROUP_NUM", "grupos de regiones de esquema"),
        ("data_region_group_num", "DATA_REGION_GROUP_NUM", "grupos de regiones de datos"),
    ] {
        if let Some(v) = opt(o, key) {
            with.push(format!("{prop}={}", count(v, what)?));
        }
    }
    let with = if with.is_empty() { String::new() } else { format!(" WITH {}", with.join(", ")) };
    Ok(format!("CREATE DATABASE {}{with}", database_path(name)))
}

impl IotDbSession {
    /// The cluster's replication factors and time partition interval
    /// (`SHOW VARIABLES`).
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let Ok(t) = self.query("SHOW VARIABLES", 500).await else {
            return Ok(Vec::new());
        };
        let text = |v: &J| match v {
            J::String(s) => s.clone(),
            v => v.to_string(),
        };
        let mut out = Vec::new();
        for row in &t.rows {
            // A first column of timestamps, if any, is skipped by matching
            // the name anywhere but the value.
            let (Some(k), Some(v)) = (row.iter().rev().nth(1), row.last()) else { continue };
            let key = match text(k).as_str() {
                "SchemaReplicationFactor" => "schema_replication_factor",
                "DataReplicationFactor" => "data_replication_factor",
                "TimePartitionInterval" => "time_partition_interval",
                _ => continue,
            };
            out.push(FieldChoices { key: key.into(), default: Some(text(v)), values: Vec::new() });
        }
        Ok(out)
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()));
        }
        self.non_query(&script(name, o)?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_name_is_the_old_create() {
        assert_eq!(script("planta", &o(&[("ttl", " ")])).unwrap(), "CREATE DATABASE root.planta");
    }

    #[test]
    fn every_option() {
        assert_eq!(
            script(
                "planta",
                &o(&[
                    ("ttl", "1h"),
                    ("time_partition_interval", "1D"),
                    ("schema_region_group_num", "1"),
                    ("data_region_group_num", "2"),
                    ("schema_replication_factor", "1"),
                    ("data_replication_factor", "3"),
                ])
            )
            .unwrap(),
            "CREATE DATABASE root.planta WITH TTL=3600000, SCHEMA_REPLICATION_FACTOR=1, DATA_REPLICATION_FACTOR=3, \
             TIME_PARTITION_INTERVAL=86400000, SCHEMA_REGION_GROUP_NUM=1, DATA_REGION_GROUP_NUM=2"
        );
        assert_eq!(script("p", &o(&[("ttl", "604800000")])).unwrap(), "CREATE DATABASE root.p WITH TTL=604800000");
    }

    #[test]
    fn values_are_checked() {
        for bad in [("ttl", "0"), ("ttl", "1y"), ("ttl", "-5"), ("ttl", "1h, DATA_REPLICATION_FACTOR=9"), ("data_region_group_num", "0"), ("schema_replication_factor", "x")] {
            assert!(script("p", &o(&[bad])).is_err(), "{bad:?}");
        }
    }
}
