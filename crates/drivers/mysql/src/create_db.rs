//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]).
//!
//! - MySQL (Aurora, Cloud SQL), MariaDB, TiDB and OceanBase: character set
//!   and collation; MariaDB adds a comment and TiDB a placement policy.
//! - SingleStore: the number of partitions.
//! - StarRocks (`storage_volume`) and Doris / VeloDB (`replication_num`):
//!   their `PROPERTIES`, plus any other `key=value` the user writes.
//! - GreptimeDB: the default TTL of the database's tables (`WITH (ttl)`).
//! - Databend (its `ENGINE` has a single useful value) and Manticore (no
//!   databases): none.
//!
//! Everything is one `CREATE DATABASE`, and every value is checked before
//! it reaches the SQL.

use crate::session::{at, lit, MySqlSession};
use crate::{err, Variant};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use mysql_async::prelude::Queryable;
use std::collections::BTreeMap;

fn charset_fields() -> Vec<Field> {
    vec![
        Field::new("charset", "Juego de caracteres (character set)", FieldKind::Text)
            .help("Vacío: el del servidor. Es el que toman las tablas nuevas."),
        Field::new("collation", "Intercalación (collation)", FieldKind::Text)
            .help("Vacía: la del juego de caracteres. Define cómo se ordenan y comparan los textos."),
    ]
}

fn properties_field() -> Field {
    Field::new("properties", "Otras propiedades", FieldKind::Textarea)
        .placeholder("clave=valor (una por línea)")
        .help("Van en PROPERTIES tal como las escribís.")
}

pub(crate) fn fields(v: Variant) -> Vec<Field> {
    match v.base() {
        Variant::MySql | Variant::OceanBase => charset_fields(),
        Variant::MariaDb => {
            let mut f = charset_fields();
            f.push(Field::new("comment", "Comentario", FieldKind::Text));
            f
        }
        Variant::TiDb => {
            let mut f = charset_fields();
            f.push(
                Field::new("placement_policy", "Política de ubicación (placement policy)", FieldKind::Text)
                    .help("Vacía: sin política. Decide en qué regiones o nodos quedan las réplicas."),
            );
            f
        }
        Variant::SingleStore => vec![Field::new("partitions", "Particiones", FieldKind::Number)
            .help("Vacío: las del clúster (por defecto, una por núcleo de las hojas).")],
        Variant::StarRocks => vec![
            Field::new("storage_volume", "Volumen de almacenamiento (storage_volume)", FieldKind::Text)
                .help("Solo en clústeres de datos compartidos. Vacío: el volumen por defecto."),
            properties_field(),
        ],
        Variant::Doris => vec![
            Field::new("replication_num", "Réplicas (replication_num)", FieldKind::Number)
                .help("Vacío: las del clúster. Es el valor por defecto de las tablas nuevas."),
            properties_field(),
        ],
        Variant::GreptimeDb => vec![Field::new("ttl", "Retención (TTL)", FieldKind::Text)
            .placeholder("7d, 24h, forever")
            .help("Vacía: sin vencimiento. Es la de las tablas que no indiquen otra.")],
        _ => Vec::new(),
    }
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn check(ok: bool, what: &str, v: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Query(format!("{what}: «{v}» no es un valor válido")))
    }
}

fn word(v: &str) -> bool {
    !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn number(v: &str) -> bool {
    !v.is_empty() && v.len() <= 6 && v.chars().all(|c| c.is_ascii_digit()) && v.parse::<u32>().is_ok_and(|n| n > 0)
}

/// `"key" = "value"` pairs for `PROPERTIES`, the named field first, then one
/// `key=value` per line of `properties`.
fn properties(named: Option<(&str, String)>, o: &BTreeMap<String, String>) -> Result<String> {
    let mut pairs: Vec<(String, String)> = named.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    for line in opt(o, "properties").unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| Error::Query(format!("propiedades: «{line}» no tiene la forma clave=valor")))?;
        let (k, v) = (k.trim().trim_matches('"'), v.trim().trim_matches('"'));
        check(!k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')), "propiedad", k)?;
        check(!v.is_empty() && !v.contains(['"', '\\', '\'']) && !v.chars().any(char::is_control), "valor de la propiedad", v)?;
        pairs.push((k.to_string(), v.to_string()));
    }
    if pairs.is_empty() {
        return Ok(String::new());
    }
    let list: Vec<String> = pairs.iter().map(|(k, v)| format!("\"{k}\" = \"{v}\"")).collect();
    Ok(format!("\nPROPERTIES ({})", list.join(", ")))
}

/// The `CREATE DATABASE` that makes `name` with `o`.
pub(crate) fn script(v: Variant, name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    if v.base() == Variant::Manticore {
        return Err(Error::Unsupported("Manticore no tiene bases de datos".into()));
    }
    let mut sql = format!("CREATE DATABASE {}", quote_ident(Quote::Backtick, name));
    match v.base() {
        Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase => {
            if let Some(c) = opt(o, "charset") {
                check(word(c), "juego de caracteres", c)?;
                sql.push_str(&format!("\nCHARACTER SET {c}"));
            }
            if let Some(c) = opt(o, "collation") {
                check(word(c), "intercalación", c)?;
                sql.push_str(&format!("\nCOLLATE {c}"));
            }
            if v.base() == Variant::MariaDb {
                if let Some(c) = opt(o, "comment") {
                    sql.push_str(&format!("\nCOMMENT {}", lit(c)));
                }
            }
            if v.base() == Variant::TiDb {
                if let Some(p) = opt(o, "placement_policy") {
                    sql.push_str(&format!("\nPLACEMENT POLICY = {}", quote_ident(Quote::Backtick, p)));
                }
            }
        }
        Variant::SingleStore => {
            if let Some(p) = opt(o, "partitions") {
                check(number(p), "particiones", p)?;
                sql.push_str(&format!("\nPARTITIONS {p}"));
            }
        }
        Variant::StarRocks => {
            let volume = match opt(o, "storage_volume") {
                Some(s) => {
                    check(word(s), "volumen de almacenamiento", s)?;
                    Some(("storage_volume", s.to_string()))
                }
                None => None,
            };
            sql.push_str(&properties(volume, o)?);
        }
        Variant::Doris => {
            let replicas = match opt(o, "replication_num") {
                Some(n) => {
                    check(number(n), "réplicas", n)?;
                    Some(("replication_num", n.to_string()))
                }
                None => None,
            };
            sql.push_str(&properties(replicas, o)?);
        }
        Variant::GreptimeDb => {
            if let Some(t) = opt(o, "ttl") {
                check(t.chars().all(|c| c.is_ascii_alphanumeric() || c == ' '), "retención", t)?;
                sql.push_str(&format!("\nWITH (ttl = {})", lit(t)));
            }
        }
        _ => {}
    }
    Ok(sql)
}

impl MySqlSession {
    /// The server's character sets and collations (default: the server's),
    /// TiDB's placement policies and StarRocks's storage volumes.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let mut out = Vec::new();
        match self.variant {
            Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase => {
                let defaults = self.optional_rows("SELECT @@character_set_server, @@collation_server").await;
                let (charset, collation) = defaults.first().map_or((None, None), |r| (at(r, 0), at(r, 1)));
                let charsets = self.optional_rows("SELECT CHARACTER_SET_NAME FROM information_schema.CHARACTER_SETS ORDER BY 1").await;
                out.push(FieldChoices { key: "charset".into(), default: charset, values: charsets.iter().filter_map(|r| at(r, 0)).collect() });
                let collations = self.optional_rows("SELECT COLLATION_NAME FROM information_schema.COLLATIONS ORDER BY 1").await;
                out.push(FieldChoices { key: "collation".into(), default: collation, values: collations.iter().filter_map(|r| at(r, 0)).collect() });
                if self.variant == Variant::TiDb {
                    let policies = self.optional_rows("SELECT POLICY_NAME FROM information_schema.PLACEMENT_POLICIES ORDER BY 1").await;
                    out.push(FieldChoices { key: "placement_policy".into(), default: None, values: policies.iter().filter_map(|r| at(r, 0)).collect() });
                }
            }
            Variant::StarRocks => {
                let volumes = self.optional_rows("SHOW STORAGE VOLUMES").await;
                out.push(FieldChoices { key: "storage_volume".into(), default: None, values: volumes.iter().filter_map(|r| at(r, 0)).collect() });
            }
            _ => {}
        }
        Ok(out)
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        if !crate::design::capabilities(self.variant).create_database {
            return Err(Error::Unsupported("este motor no crea bases desde DBine".into()));
        }
        let sql = script(self.variant, name, o)?;
        self.conn.query_drop(sql).await.map_err(err)
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
        for v in Variant::ALL.into_iter().filter(|v| *v != Variant::Manticore) {
            assert_eq!(script(v, "ven`tas", &o(&[("charset", " ")])).unwrap(), "CREATE DATABASE `ven``tas`", "{v:?}");
        }
        assert!(script(Variant::Manticore, "v", &o(&[])).is_err());
        assert!(fields(Variant::Manticore).is_empty() && fields(Variant::Databend).is_empty());
    }

    #[test]
    fn charset_collation_and_extras() {
        let all = o(&[
            ("charset", "utf8mb4"),
            ("collation", "utf8mb4_bin"),
            ("comment", "it's \\ here"),
            ("placement_policy", "p1"),
        ]);
        assert_eq!(script(Variant::MySql, "v", &all).unwrap(), "CREATE DATABASE `v`\nCHARACTER SET utf8mb4\nCOLLATE utf8mb4_bin");
        assert_eq!(script(Variant::AuroraMySql, "v", &all).unwrap(), script(Variant::MySql, "v", &all).unwrap());
        assert_eq!(
            script(Variant::MariaDb, "v", &all).unwrap(),
            "CREATE DATABASE `v`\nCHARACTER SET utf8mb4\nCOLLATE utf8mb4_bin\nCOMMENT 'it''s \\\\ here'"
        );
        assert_eq!(
            script(Variant::TiDb, "v", &all).unwrap(),
            "CREATE DATABASE `v`\nCHARACTER SET utf8mb4\nCOLLATE utf8mb4_bin\nPLACEMENT POLICY = `p1`"
        );
        assert_eq!(script(Variant::SingleStore, "v", &o(&[("partitions", "16")])).unwrap(), "CREATE DATABASE `v`\nPARTITIONS 16");
        assert_eq!(script(Variant::GreptimeDb, "v", &o(&[("ttl", "7d")])).unwrap(), "CREATE DATABASE `v`\nWITH (ttl = '7d')");
    }

    #[test]
    fn properties() {
        assert_eq!(
            script(Variant::Doris, "v", &o(&[("replication_num", "1"), ("properties", "storage_vault_name = v1\n\n\"x.y\"=\"z\"")])).unwrap(),
            "CREATE DATABASE `v`\nPROPERTIES (\"replication_num\" = \"1\", \"storage_vault_name\" = \"v1\", \"x.y\" = \"z\")"
        );
        assert_eq!(script(Variant::VeloDb, "v", &o(&[("replication_num", "3")])).unwrap(), "CREATE DATABASE `v`\nPROPERTIES (\"replication_num\" = \"3\")");
        assert_eq!(
            script(Variant::StarRocks, "v", &o(&[("storage_volume", "builtin_storage_volume")])).unwrap(),
            "CREATE DATABASE `v`\nPROPERTIES (\"storage_volume\" = \"builtin_storage_volume\")"
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in [("charset", "utf8; DROP"), ("collation", "a b")] {
            assert!(script(Variant::MySql, "v", &o(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(Variant::SingleStore, "v", &o(&[("partitions", "0")])).is_err());
        assert!(script(Variant::SingleStore, "v", &o(&[("partitions", "8 ")])).is_ok());
        assert!(script(Variant::SingleStore, "v", &o(&[("partitions", "-1")])).is_err());
        assert!(script(Variant::Doris, "v", &o(&[("replication_num", "x")])).is_err());
        assert!(script(Variant::Doris, "v", &o(&[("properties", "a=\"b\"\")")])).is_err());
        assert!(script(Variant::Doris, "v", &o(&[("properties", "novalue")])).is_err());
        assert!(script(Variant::StarRocks, "v", &o(&[("properties", "a b=c")])).is_err());
        assert!(script(Variant::StarRocks, "v", &o(&[("storage_volume", "x\")")])).is_err());
        assert!(script(Variant::GreptimeDb, "v", &o(&[("ttl", "7d')")])).is_err());
    }
}
