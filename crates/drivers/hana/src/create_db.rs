//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]).
//!
//! The "databases" below a HANA connection are schemas, and `CREATE SCHEMA`
//! takes one option: its owner (`OWNED BY`). Tenant databases are created
//! from the system database's cockpit, not from a tenant connection.

use crate::{quote, text, HanaSession};
use dbine_driver::{Field, FieldChoices, FieldKind, Result};
use std::collections::BTreeMap;

pub(crate) fn fields() -> Vec<Field> {
    vec![Field::new("owner", "Dueño (OWNED BY)", FieldKind::Text).help("Vacío: el usuario con el que estás conectado.")]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// A user's name: simple identifiers in upper case, as HANA folds them
/// unquoted; anything else exactly as written.
fn user(name: &str) -> String {
    let simple = name.starts_with(|c: char| c.is_ascii_alphabetic()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    quote(&if simple { name.to_ascii_uppercase() } else { name.to_string() })
}

pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let mut sql = format!("CREATE SCHEMA {}", quote(name.trim()));
    if let Some(owner) = opt(o, "owner") {
        sql.push_str(&format!(" OWNED BY {}", user(owner)));
    }
    Ok(sql)
}

impl HanaSession {
    /// The active users (default: the one connected).
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let users = self
            .rows("SELECT USER_NAME FROM SYS.USERS WHERE USER_DEACTIVATED = 'FALSE' ORDER BY USER_NAME", &[])
            .await
            .unwrap_or_default();
        let me = self.rows("SELECT CURRENT_USER FROM DUMMY", &[]).await.ok().and_then(|r| r.first().and_then(|r| r.first()).and_then(text));
        Ok(vec![FieldChoices { key: "owner".into(), default: me, values: users.iter().filter_map(|r| r.first().and_then(text)).collect() }])
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        let sql = script(name, o)?;
        self.conn.exec(sql).await.map_err(crate::err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_and_owner() {
        assert_eq!(script(" ventas ", &o(&[("owner", "")])).unwrap(), "CREATE SCHEMA \"ventas\"");
        assert_eq!(script("V", &o(&[("owner", "dbadmin")])).unwrap(), "CREATE SCHEMA \"V\" OWNED BY \"DBADMIN\"");
        // Odd names are quoted as written, never loose.
        assert_eq!(script("V", &o(&[("owner", "a\" b")])).unwrap(), "CREATE SCHEMA \"V\" OWNED BY \"a\"\" b\"");
    }
}
