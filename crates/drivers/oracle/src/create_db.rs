//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]).
//!
//! A "database" here is a schema-only account (`CREATE USER … NO
//! AUTHENTICATION`), so its options are the account's storage: default and
//! temporary tablespace and the quota on the default one (unlimited when
//! empty, as the plain create does).
//!
//! Without a default tablespace the quota goes on the database's default
//! permanent tablespace, whose name only the server knows: the statement
//! is built inside an anonymous block that reads it from
//! `DATABASE_PROPERTIES`, the same lookup the plain create makes.

use crate::{err, quote, schema_name, strings, OracleSession};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use std::collections::BTreeMap;

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new("default_tablespace", "Tablespace por defecto", FieldKind::Text)
            .help("Vacío: el tablespace permanente por defecto de la base. Ahí van las tablas del esquema."),
        Field::new("temporary_tablespace", "Tablespace temporal", FieldKind::Text)
            .help("Vacío: el temporal por defecto de la base."),
        Field::new("quota", "Cuota en el tablespace por defecto", FieldKind::Text)
            .placeholder("UNLIMITED, 500M o 10G")
            .help("Vacía: UNLIMITED."),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// `UNLIMITED`, or a size: digits with an optional K / M / G / T.
pub(crate) fn quota(v: &str) -> Result<String> {
    let s = v.replace(' ', "").to_ascii_uppercase();
    if s == "UNLIMITED" {
        return Ok(s);
    }
    let digits = s.trim_end_matches(['K', 'M', 'G', 'T']);
    let unit = &s[digits.len()..];
    if !digits.is_empty() && digits.len() <= 12 && digits.chars().all(|c| c.is_ascii_digit()) && unit.len() <= 1 {
        Ok(s)
    } else {
        Err(Error::Query(format!("cuota: «{v}» no es UNLIMITED ni un tamaño (500M, 10G…)")))
    }
}

/// A tablespace's name, folded like an unquoted identifier.
pub(crate) fn tablespace(v: &str) -> Result<String> {
    Ok(quote(&schema_name(v)?))
}

/// The code that creates schema `name`: one `CREATE USER`, or the
/// anonymous block that finds the default tablespace first.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let user = quote(&schema_name(name)?);
    let mut sql = format!("CREATE USER {user} NO AUTHENTICATION");
    let default = opt(o, "default_tablespace").map(tablespace).transpose()?;
    if let Some(ts) = &default {
        sql.push_str(&format!("\nDEFAULT TABLESPACE {ts}"));
    }
    if let Some(t) = opt(o, "temporary_tablespace") {
        sql.push_str(&format!("\nTEMPORARY TABLESPACE {}", tablespace(t)?));
    }
    let quota = opt(o, "quota").map(quota).transpose()?.unwrap_or_else(|| "UNLIMITED".into());
    if let Some(ts) = &default {
        sql.push_str(&format!("\nQUOTA {quota} ON {ts}"));
        return Ok(sql);
    }
    // Names were checked and quoted; inside the block they go in a string
    // literal, so single quotes double.
    let stmt = sql.replace('\'', "''").replace('\n', " ");
    Ok(format!(
        "DECLARE
  ts VARCHAR2(128);
BEGIN
  SELECT MAX(property_value) INTO ts FROM database_properties WHERE property_name = 'DEFAULT_PERMANENT_TABLESPACE';
  EXECUTE IMMEDIATE '{stmt}'
    || CASE WHEN ts IS NOT NULL THEN ' QUOTA {quota} ON \"' || REPLACE(ts, '\"', '\"\"') || '\"' END;
END;"
    ))
}

impl OracleSession {
    /// Permanent and temporary tablespaces (default: the database's) and
    /// the quota the plain create gives.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        self.run(|c| {
            let list = |contents: &str| {
                // DBA_TABLESPACES needs a privilege; USER_TABLESPACES lists
                // the ones this user can see.
                let q = |view: &str| format!("SELECT tablespace_name FROM {view} WHERE contents = '{contents}' ORDER BY 1");
                c.query(&q("dba_tablespaces"), &[])
                    .or_else(|_| c.query(&q("user_tablespaces"), &[]))
                    .map_err(err)
                    .and_then(strings)
                    .unwrap_or_default()
            };
            let prop = |p: &str| -> Option<String> {
                c.query_row("SELECT property_value FROM database_properties WHERE property_name = :1", &[&p])
                    .and_then(|r| r.get(0))
                    .unwrap_or(None)
            };
            Ok(vec![
                FieldChoices { key: "default_tablespace".into(), default: prop("DEFAULT_PERMANENT_TABLESPACE"), values: list("PERMANENT") },
                FieldChoices { key: "temporary_tablespace".into(), default: prop("DEFAULT_TEMP_TABLESPACE"), values: list("TEMPORARY") },
                FieldChoices { key: "quota".into(), default: Some("UNLIMITED".into()), values: vec!["UNLIMITED".into()] },
            ])
        })
        .await
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        let sql = script(name, o)?;
        self.run(move |c| c.execute(&sql, &[]).map(|_| ()).map_err(err)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_name_reads_the_default_tablespace() {
        let s = script("ventas", &o(&[("quota", " ")])).unwrap();
        assert!(s.starts_with("DECLARE"), "{s}");
        assert!(s.contains("EXECUTE IMMEDIATE 'CREATE USER \"VENTAS\" NO AUTHENTICATION'"), "{s}");
        assert!(s.contains("' QUOTA UNLIMITED ON \"' || REPLACE(ts, '\"', '\"\"') || '\"'"), "{s}");
        // A quoted name with a single quote stays one literal.
        assert!(script("it's", &o(&[])).unwrap().contains("'CREATE USER \"it''s\" NO AUTHENTICATION'"));
    }

    #[test]
    fn named_tablespaces_are_one_statement() {
        assert_eq!(
            script("ventas", &o(&[("default_tablespace", "users"), ("temporary_tablespace", "TEMP"), ("quota", "500 m")])).unwrap(),
            "CREATE USER \"VENTAS\" NO AUTHENTICATION\nDEFAULT TABLESPACE \"USERS\"\nTEMPORARY TABLESPACE \"TEMP\"\nQUOTA 500M ON \"USERS\""
        );
        assert_eq!(
            script("v", &o(&[("default_tablespace", "DATA")])).unwrap(),
            "CREATE USER \"V\" NO AUTHENTICATION\nDEFAULT TABLESPACE \"DATA\"\nQUOTA UNLIMITED ON \"DATA\""
        );
        let s = script("v", &o(&[("temporary_tablespace", "temp2"), ("quota", "10G")])).unwrap();
        assert!(s.contains("'CREATE USER \"V\" NO AUTHENTICATION TEMPORARY TABLESPACE \"TEMP2\"'") && s.contains("QUOTA 10G ON"), "{s}");
    }

    #[test]
    fn values_are_checked() {
        for bad in ["10X", "M", "1.5G", "10 GB", "UNLIMITED;"] {
            assert!(script("v", &o(&[("quota", bad)])).is_err(), "{bad}");
        }
        assert!(script("v", &o(&[("quota", "0")])).is_ok());
        // An odd tablespace name is quoted, never loose.
        assert!(script("v", &o(&[("default_tablespace", "a\" b")])).unwrap().contains("DEFAULT TABLESPACE \"a\"\" b\""));
        assert!(script(" ", &o(&[])).is_err());
    }
}
