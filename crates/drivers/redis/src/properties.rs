//! "Propiedades" of a logical database (`db0`…)
//! ([`dbine_driver::Session::database_properties`]): facts only, from
//! `INFO keyspace`. Redis, Valkey and Dragonfly keep no settings per
//! database: `CONFIG SET` changes the whole server.

use crate::{db_index, shape, RedisSession};
use dbine_driver::{DatabaseProperties, PropertyInfo, Result};

/// The `dbN:keys=…,expires=…,avg_ttl=…` line of `INFO keyspace` for `db`,
/// as facts. A database with no keys has no line.
pub(crate) fn facts(keyspace: &str, db: i64) -> Vec<PropertyInfo> {
    let prefix = format!("db{db}:");
    let line = keyspace.lines().find_map(|l| l.trim().strip_prefix(prefix.as_str())).unwrap_or("");
    let field = |k: &str| line.split(',').find_map(|p| p.split_once('=').filter(|(n, _)| *n == k).map(|(_, v)| v.to_string()));
    let mut out = Vec::new();
    let mut fact = |label: &str, value: String| out.push(PropertyInfo { group: String::new(), label: label.into(), value });
    fact("Claves", field("keys").unwrap_or_else(|| "0".into()));
    fact("Claves con vencimiento (expires)", field("expires").unwrap_or_else(|| "0".into()));
    if let Some(ttl) = field("avg_ttl").and_then(|v| v.parse::<u64>().ok()).filter(|t| *t > 0) {
        fact("Vencimiento promedio (avg_ttl)", format!("{:.1} s", ttl as f64 / 1000.0));
    }
    // Redis 7.4+: hashes with fields that expire (HEXPIRE).
    if let Some(n) = field("subexpiry").filter(|v| v != "0") {
        fact("Hashes con campos que vencen (subexpiry)", n);
    }
    out
}

impl RedisSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let db = db_index(database)?;
        let keyspace = shape::text_of(&self.run(&[b"INFO", b"keyspace"]).await?);
        Ok(DatabaseProperties { info: facts(&keyspace, db), ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_from_info_keyspace() {
        let info = "# Keyspace\r\ndb0:keys=12,expires=3,avg_ttl=45000,subexpiry=0\r\ndb10:keys=1,expires=0,avg_ttl=0\r\n";
        let f = facts(info, 0);
        let v: Vec<(&str, &str)> = f.iter().map(|i| (i.label.as_str(), i.value.as_str())).collect();
        assert_eq!(v, vec![("Claves", "12"), ("Claves con vencimiento (expires)", "3"), ("Vencimiento promedio (avg_ttl)", "45.0 s")]);
        // db1 isn't db10, and an empty database has no line.
        assert_eq!(facts(info, 1)[0].value, "0");
        assert_eq!(facts(info, 10)[0].value, "1");
    }
}
