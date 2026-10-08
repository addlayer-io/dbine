//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]).
//!
//! A "database" below a HANA connection is a schema, and HANA has no
//! `ALTER SCHEMA`: its owner is fixed at `CREATE SCHEMA … OWNED BY`, and
//! settings such as the load unit live on tables, partitions and columns,
//! not on the schema. So "Propiedades" shows facts only: owner, created,
//! how many tables, views, procedures and functions it has, and the size
//! of its column-store tables in memory and on disk.

use crate::{text, HanaSession};
use dbine_driver::{DatabaseProperties, Error, PropertyInfo, Result};
use std::collections::BTreeMap;

/// The statements for `changes`: none can be made.
pub(crate) fn alter(changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    match changes.keys().next() {
        None => Ok(Vec::new()),
        Some(k) => Err(Error::Query(format!(
            "propiedad desconocida: {k} (HANA no permite cambiar un esquema después de crearlo; el dueño se fija al crearlo)"
        ))),
    }
}

pub(crate) fn script(changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(changes)?.join(";\n"))
}

/// `1.5 GB`.
fn human(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", bytes as u64)
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

impl HanaSession {
    /// The first cell of an optional catalog query, as text.
    async fn cell(&self, sql: &str, schema: &str) -> Option<String> {
        self.rows(sql, &[schema]).await.ok()?.first()?.first().and_then(text)
    }

    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let rows = self.rows("SELECT SCHEMA_OWNER FROM SYS.SCHEMAS WHERE SCHEMA_NAME = ?", &[database]).await?;
        let owner = rows.first().ok_or_else(|| Error::Query(format!("no existe el esquema «{database}»")))?.first().and_then(text);
        let fact = |group: &str, label: &str, value: String| PropertyInfo { group: group.into(), label: label.into(), value };
        let mut info = vec![fact("", "Dueño", owner.unwrap_or_default())];
        if let Some(c) = self.cell("SELECT TO_VARCHAR(CREATE_TIME, 'YYYY-MM-DD HH24:MI:SS') FROM SYS.SCHEMAS WHERE SCHEMA_NAME = ?", database).await {
            info.push(fact("", "Creado", c));
        }
        for (label, view) in [("Tablas", "TABLES"), ("Vistas", "VIEWS"), ("Procedimientos", "PROCEDURES"), ("Funciones", "FUNCTIONS")] {
            if let Some(n) = self.cell(&format!("SELECT TO_VARCHAR(COUNT(*)) FROM SYS.{view} WHERE SCHEMA_NAME = ?"), database).await {
                info.push(fact("", label, n));
            }
        }
        let size = |v: Option<String>| v.and_then(|s| s.parse::<f64>().ok()).map(human);
        let memory = self
            .cell(
                "SELECT TO_VARCHAR(COALESCE(SUM(MEMORY_SIZE_IN_TOTAL), 0)) FROM SYS.M_CS_TABLES WHERE SCHEMA_NAME = ?",
                database,
            )
            .await;
        if let Some(m) = size(memory) {
            info.push(fact("Almacenamiento", "En memoria (column store)", m));
        }
        if let Some(r) = self.cell("SELECT TO_VARCHAR(COALESCE(SUM(RECORD_COUNT), 0)) FROM SYS.M_CS_TABLES WHERE SCHEMA_NAME = ?", database).await {
            info.push(fact("Almacenamiento", "Filas (column store)", r));
        }
        if let Some(d) = size(self.cell("SELECT TO_VARCHAR(COALESCE(SUM(DISK_SIZE), 0)) FROM SYS.M_TABLE_PERSISTENCE_STATISTICS WHERE SCHEMA_NAME = ?", database).await) {
            info.push(fact("Almacenamiento", "En disco", d));
        }
        Ok(DatabaseProperties { info, ..Default::default() })
    }

    pub(crate) async fn alter_database_impl(&mut self, _database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        alter(changes).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_change() {
        assert_eq!(script(&BTreeMap::new()).unwrap(), "");
        assert!(script(&[("owner".to_string(), "X".to_string())].into()).is_err());
        assert_eq!(human(1536.0), "1.5 KB");
        assert_eq!(human(10.0), "10 B");
    }
}
