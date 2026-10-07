//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]).
//!
//! ClickHouse: the database engine (Atomic, Replicated with its Keeper path,
//! shard and replica, Memory), `ON CLUSTER` and `COMMENT`, all in the one
//! `CREATE DATABASE`. Lazy is left out: recent servers (26.x) dropped it.
//! The engines that mirror another server (MySQL, PostgreSQL, S3…) take a
//! connection, not options, and stay out too. Timeplus Proton's
//! `CREATE DATABASE` takes only the name: no options.
//!
//! Every value is checked before it reaches the SQL.

use crate::{text, ClickHouseSession, Flavor};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use std::collections::BTreeMap;

pub(crate) fn fields(flavor: Flavor) -> Vec<Field> {
    if flavor == Flavor::Timeplus {
        return Vec::new();
    }
    vec![
        Field::new(
            "engine",
            "Motor (ENGINE)",
            FieldKind::Select(vec![
                ("Atomic", "Atomic"),
                ("Replicated", "Replicated (réplicas vía Keeper)"),
                ("Memory", "Memory (sin persistencia)"),
            ]),
        )
        .help("Vacío: el del servidor (Atomic)."),
        Field::new("zoo_path", "Ruta en Keeper (zoo_path)", FieldKind::Text)
            .placeholder("/clickhouse/databases/{uuid}")
            .help("Vacía: la configurada en el servidor (database_replicated_default_zk_path).")
            .when("engine", &["Replicated"]),
        Field::new("shard", "Shard", FieldKind::Text).placeholder("{shard}").when("engine", &["Replicated"]),
        Field::new("replica", "Réplica", FieldKind::Text).placeholder("{replica}").when("engine", &["Replicated"]),
        Field::new("cluster", "Clúster (ON CLUSTER)", FieldKind::Text).help("Crea la base en todos los nodos del clúster."),
        Field::new("comment", "Comentario", FieldKind::Textarea),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

/// A ClickHouse string literal (backslash escapes, as the server reads it).
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Keeper paths, shard and replica names: letters, digits, `/ _ - .` and
/// `{macro}`s.
fn keeper_text(v: &str, path: bool) -> bool {
    v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '{' | '}') || (path && c == '/'))
}

/// The `CREATE DATABASE` for `name`.
pub(crate) fn script(flavor: Flavor, name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let mut sql = format!("CREATE DATABASE {}", quote_ident(Quote::Backtick, name.trim()));
    if flavor == Flavor::Timeplus {
        return Ok(sql);
    }
    if let Some(c) = opt(o, "cluster") {
        sql.push_str(&format!(" ON CLUSTER {}", quote_ident(Quote::Backtick, c)));
    }
    match opt(o, "engine") {
        None => {}
        Some(e @ ("Atomic" | "Memory")) => sql.push_str(&format!("\nENGINE = {e}")),
        Some("Replicated") => {
            let (path, shard, replica) = (opt(o, "zoo_path"), opt(o, "shard"), opt(o, "replica"));
            match path {
                None if shard.is_some() || replica.is_some() => {
                    return Err(Error::Query("para elegir shard o réplica, indicá también la ruta en Keeper".into()));
                }
                None => sql.push_str("\nENGINE = Replicated"),
                Some(p) => {
                    if !p.starts_with('/') || !keeper_text(p, true) {
                        return Err(bad("ruta en Keeper", p));
                    }
                    let shard = shard.unwrap_or("{shard}");
                    let replica = replica.unwrap_or("{replica}");
                    for (what, v) in [("shard", shard), ("réplica", replica)] {
                        if !keeper_text(v, false) {
                            return Err(bad(what, v));
                        }
                    }
                    sql.push_str(&format!("\nENGINE = Replicated({}, {}, {})", literal(p), literal(shard), literal(replica)));
                }
            }
        }
        Some(e) => return Err(bad("motor", e)),
    }
    if let Some(c) = opt(o, "comment") {
        sql.push_str(&format!("\nCOMMENT {}", literal(c)));
    }
    Ok(sql)
}

impl ClickHouseSession {
    /// The clusters in `system.clusters`, the default database engine and
    /// the server's default Keeper path for Replicated.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        if self.flavor == Flavor::Timeplus {
            return Ok(Vec::new());
        }
        let first = |rows: Vec<Vec<serde_json::Value>>| rows.first().and_then(|r| r.first()).map(text).filter(|v| !v.is_empty());
        let clusters: Vec<String> = self
            .rows("SELECT DISTINCT cluster FROM system.clusters ORDER BY cluster", &[])
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|r| r.first().map(text))
            .collect();
        let engine = self.rows("SELECT value FROM system.settings WHERE name = 'default_database_engine'", &[]).await.ok().and_then(first);
        let zoo = self
            .rows("SELECT value FROM system.server_settings WHERE name = 'database_replicated_default_zk_path'", &[])
            .await
            .ok()
            .and_then(first);
        Ok(vec![
            FieldChoices { key: "engine".into(), default: engine.or_else(|| Some("Atomic".into())), values: Vec::new() },
            FieldChoices { key: "zoo_path".into(), default: zoo, values: Vec::new() },
            FieldChoices { key: "cluster".into(), default: None, values: clusters },
        ])
    }

    /// One statement; `ON CLUSTER` answers with each host's status, read
    /// to the end so a failing host surfaces.
    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        let sql = script(self.flavor, name, o)?;
        self.rows(&sql, &[]).await.map(|_| ())
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
        assert_eq!(script(Flavor::ClickHouse, "ventas", &o(&[])).unwrap(), "CREATE DATABASE `ventas`");
        assert_eq!(script(Flavor::ClickHouse, "ven`tas", &o(&[("comment", " ")])).unwrap(), "CREATE DATABASE `ven``tas`");
        assert_eq!(script(Flavor::Timeplus, "v", &o(&[("engine", "Memory"), ("comment", "x")])).unwrap(), "CREATE DATABASE `v`");
        assert!(fields(Flavor::Timeplus).is_empty());
    }

    #[test]
    fn engine_cluster_and_comment() {
        assert_eq!(
            script(
                Flavor::ClickHouse,
                "v",
                &o(&[("engine", "Replicated"), ("zoo_path", "/clickhouse/db/{uuid}"), ("replica", "r-1"), ("cluster", "prod"), ("comment", "it's \\ ok")])
            )
            .unwrap(),
            "CREATE DATABASE `v` ON CLUSTER `prod`\nENGINE = Replicated('/clickhouse/db/{uuid}', '{shard}', 'r-1')\nCOMMENT 'it\\'s \\\\ ok'"
        );
        assert_eq!(script(Flavor::ClickHouse, "v", &o(&[("engine", "Replicated")])).unwrap(), "CREATE DATABASE `v`\nENGINE = Replicated");
        assert_eq!(script(Flavor::ClickHouse, "v", &o(&[("engine", "Atomic")])).unwrap(), "CREATE DATABASE `v`\nENGINE = Atomic");
        // Options of another engine are ignored.
        assert_eq!(script(Flavor::ClickHouse, "v", &o(&[("engine", "Memory"), ("zoo_path", "x")])).unwrap(), "CREATE DATABASE `v`\nENGINE = Memory");
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            &[("engine", "MySQL('h', 'db')")][..],
            &[("engine", "Lazy")],
            &[("engine", "Replicated"), ("zoo_path", "relative/path")],
            &[("engine", "Replicated"), ("zoo_path", "/a'b")],
            &[("engine", "Replicated"), ("zoo_path", "/a"), ("shard", "s')")],
            &[("engine", "Replicated"), ("replica", "r1")],
        ] {
            assert!(script(Flavor::ClickHouse, "v", &o(bad)).is_err(), "{bad:?}");
        }
    }
}
