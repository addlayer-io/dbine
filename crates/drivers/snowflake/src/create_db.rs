//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]):
//! `TRANSIENT`, Time Travel retention, the maximum data extension, the
//! default DDL collation and the comment, all in the one `CREATE DATABASE`.
//!
//! Every value is checked before it reaches the SQL.

use crate::ddl::lit;
use crate::SnowflakeSession;
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use std::collections::BTreeMap;

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new("transient", "Transitoria (TRANSIENT)", FieldKind::Bool)
            .help("Sin Fail-safe y con Time Travel de 0 o 1 día: más barata para datos que se pueden regenerar."),
        Field::new("retention_days", "Días de Time Travel (DATA_RETENTION_TIME_IN_DAYS)", FieldKind::Number)
            .help("Vacío: el de la cuenta. De 0 a 90 (0 o 1 si es transitoria)."),
        Field::new("max_extension_days", "Extensión máxima de retención (MAX_DATA_EXTENSION_TIME_IN_DAYS)", FieldKind::Number)
            .help("Días que se puede extender la retención para no perder streams. De 0 a 90."),
        Field::new("collation", "Intercalación por defecto (DEFAULT_DDL_COLLATION)", FieldKind::Text)
            .placeholder("en-ci")
            .help("La de las columnas de texto que se creen sin una."),
        Field::new("comment", "Comentario", FieldKind::Textarea),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// Days between 0 and `max`.
fn days(o: &BTreeMap<String, String>, key: &str, what: &str, max: u32) -> Result<Option<u32>> {
    let Some(v) = opt(o, key) else { return Ok(None) };
    match v.parse::<u32>() {
        Ok(n) if n <= max && v.chars().all(|c| c.is_ascii_digit()) => Ok(Some(n)),
        _ => Err(Error::Query(format!("{what}: «{v}» no es un número de días entre 0 y {max}"))),
    }
}

/// The `CREATE DATABASE` for `name`.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let transient = match opt(o, "transient") {
        None | Some("false") => false,
        Some("true") => true,
        Some(v) => return Err(Error::Query(format!("transitoria: «{v}» no es un valor válido"))),
    };
    let mut sql = format!("CREATE {}DATABASE {}", if transient { "TRANSIENT " } else { "" }, qualified_name(Quote::Double, None, name));
    if let Some(n) = days(o, "retention_days", "días de Time Travel", if transient { 1 } else { 90 })? {
        sql.push_str(&format!("\nDATA_RETENTION_TIME_IN_DAYS = {n}"));
    }
    if let Some(n) = days(o, "max_extension_days", "extensión máxima de retención", 90)? {
        sql.push_str(&format!("\nMAX_DATA_EXTENSION_TIME_IN_DAYS = {n}"));
    }
    if let Some(c) = opt(o, "collation") {
        // `en-ci`, `es-ai-pi`, `utf8`, `en_US-trim`…
        if !c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
            return Err(Error::Query(format!("intercalación: «{c}» no es una especificación de collation")));
        }
        sql.push_str(&format!("\nDEFAULT_DDL_COLLATION = {}", lit(c)));
    }
    if let Some(c) = opt(o, "comment") {
        sql.push_str(&format!("\nCOMMENT = {}", lit(c)));
    }
    Ok(sql)
}

impl SnowflakeSession {
    /// The account's retention, extension and collation (what a database
    /// gets when the field is left empty), from one `SHOW PARAMETERS`.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let rows = self.named_rows("SHOW PARAMETERS IN ACCOUNT").await.unwrap_or_default();
        let value = |key: &str| {
            rows.iter()
                .find(|r| r.get("key").is_some_and(|k| k.eq_ignore_ascii_case(key)))
                .and_then(|r| r.get("value").cloned())
                .filter(|v| !v.is_empty())
        };
        Ok(vec![
            FieldChoices { key: "retention_days".into(), default: value("DATA_RETENTION_TIME_IN_DAYS"), values: Vec::new() },
            FieldChoices { key: "max_extension_days".into(), default: value("MAX_DATA_EXTENSION_TIME_IN_DAYS"), values: Vec::new() },
            FieldChoices { key: "collation".into(), default: value("DEFAULT_DDL_COLLATION"), values: Vec::new() },
        ])
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        let sql = script(name, o)?;
        self.statement(&sql, None, 1).await.map(|_| ())
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
        assert_eq!(script("Ventas", &o(&[])).unwrap(), r#"CREATE DATABASE "Ventas""#);
        assert_eq!(script("a\"b", &o(&[("transient", ""), ("comment", " ")])).unwrap(), r#"CREATE DATABASE "a""b""#);
    }

    #[test]
    fn every_option() {
        assert_eq!(
            script(
                "V",
                &o(&[("transient", "true"), ("retention_days", "1"), ("max_extension_days", "14"), ("collation", "es-ci"), ("comment", "it's \\ ok")])
            )
            .unwrap(),
            "CREATE TRANSIENT DATABASE \"V\"\nDATA_RETENTION_TIME_IN_DAYS = 1\nMAX_DATA_EXTENSION_TIME_IN_DAYS = 14\nDEFAULT_DDL_COLLATION = 'es-ci'\nCOMMENT = 'it''s \\\\ ok'"
        );
        assert_eq!(script("V", &o(&[("retention_days", "90")])).unwrap(), "CREATE DATABASE \"V\"\nDATA_RETENTION_TIME_IN_DAYS = 90");
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            &[("retention_days", "91")][..],
            &[("retention_days", "-1")],
            &[("retention_days", "1; DROP")],
            &[("transient", "true"), ("retention_days", "7")],
            &[("max_extension_days", "x")],
            &[("collation", "en' --")],
            &[("transient", "yes")],
        ] {
            assert!(script("V", &o(bad)).is_err(), "{bad:?}");
        }
    }
}
