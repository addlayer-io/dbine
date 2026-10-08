//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what Athena's `GetDatabase` reports (description and parameters), and
//! what `ALTER DATABASE … SET DBPROPERTIES` changes: each property, and new
//! ones.
//!
//! Athena's DDL can't change a database's description (comment) or
//! location, nor remove a property: the description is shown as a fact,
//! and an emptied property is set to ''. All the changes go in one
//! statement.

use crate::create_db::properties;
use crate::ddl::{lit, q};
use crate::{err, AthenaSession};
use dbine_driver::serde_static::intern;
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use std::collections::BTreeMap;

const PROPS: &str = "Propiedades (DBPROPERTIES)";

fn property_name(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

pub(crate) fn alter(database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let mut set: Vec<(String, String)> = Vec::new();
    for (key, value) in changes {
        if key == "properties_add" {
            set.extend(properties(value)?);
        } else if let Some(name) = key.strip_prefix("prop:").filter(|n| property_name(n)) {
            if value.chars().any(char::is_control) {
                return Err(Error::Query(format!("propiedades: el valor de «{name}» tiene caracteres de control")));
            }
            set.push((name.to_string(), value.trim().to_string()));
        } else {
            return Err(Error::Query(format!("propiedad desconocida: {key}")));
        }
    }
    if set.is_empty() {
        return Ok(Vec::new());
    }
    let list: Vec<String> = set.iter().map(|(k, v)| format!("{} = {}", lit(k), lit(v))).collect();
    Ok(vec![format!("ALTER DATABASE {} SET DBPROPERTIES ({})", q(database), list.join(", "))])
}

pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(database, changes)?.join(";\n"))
}

impl AthenaSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let resp = self.client.get_database().catalog_name(&self.catalog).database_name(database).send().await.map_err(err)?;
        let db = resp.database().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        let general = |label: &str, value: String| PropertyInfo { group: String::new(), label: label.into(), value };
        let mut info = vec![general("Catálogo de datos", self.catalog.clone())];
        if let Some(d) = db.description().filter(|d| !d.is_empty()) {
            info.push(general("Descripción (comentario)", d.to_string()));
        }
        let mut fields = Vec::new();
        let mut values = BTreeMap::new();
        let mut params: Vec<(&String, &String)> = db.parameters().map(|p| p.iter().collect()).unwrap_or_default();
        params.sort();
        for (k, v) in params {
            if !property_name(k) {
                info.push(PropertyInfo { group: PROPS.into(), label: k.clone(), value: v.clone() });
                continue;
            }
            let key = format!("prop:{k}");
            fields.push(Field::new(intern(&key), intern(k), FieldKind::Text).group(PROPS));
            values.insert(key, v.clone());
        }
        fields.push(
            Field::new("properties_add", "Propiedades nuevas", FieldKind::Textarea)
                .placeholder("creador=ana\nequipo=ventas")
                .help("Una por línea, como clave=valor. Athena no borra propiedades: vaciar una la deja vacía.")
                .group(PROPS),
        );
        values.insert("properties_add".into(), String::new());
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings: BTreeMap::new() })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        for sql in alter(database, changes)? {
            self.run(&sql, 1).await?;
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
    fn one_statement() {
        assert_eq!(
            script("ventas", &c(&[("prop:creador", "ana o'neil"), ("prop:equipo", ""), ("properties_add", "costo=bajo\n\nnivel = 2")])).unwrap(),
            "ALTER DATABASE `ventas` SET DBPROPERTIES ('creador' = 'ana o\\'neil', 'equipo' = '', 'costo' = 'bajo', 'nivel' = '2')"
        );
        assert_eq!(script("ventas", &c(&[("properties_add", " ")])).unwrap(), "");
    }

    #[test]
    fn values_are_checked() {
        for bad in [("prop:a b", "1"), ("prop:x", "a\nb"), ("properties_add", "sin igual"), ("properties_add", "a'b=1"), ("comment", "x")] {
            assert!(script("v", &c(&[bad])).is_err(), "{bad:?}");
        }
    }
}
