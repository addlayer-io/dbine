//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]).
//! A database here is a Unity Catalog catalog: `CREATE CATALOG` takes a
//! `MANAGED LOCATION` (where its managed tables live, instead of the
//! metastore's root) and a `COMMENT`.
//!
//! Every value is checked before it reaches the SQL.

use crate::ddl::lit;
use crate::DatabricksSession;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use std::collections::BTreeMap;

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new("managed_location", "Ubicación administrada (MANAGED LOCATION)", FieldKind::Text)
            .placeholder("s3://bucket/ruta")
            .help("Vacía: la raíz del metastore. Tiene que estar dentro de una ubicación externa."),
        Field::new("comment", "Comentario", FieldKind::Textarea),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// A cloud storage URL: `s3://`, `abfss://`, `gs://`, `r2://`…
fn storage_url(v: &str) -> bool {
    match v.split_once("://") {
        Some((scheme, rest)) => {
            !scheme.is_empty()
                && scheme.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && !rest.is_empty()
                && !v.chars().any(|c| c.is_control() || c == '\'' || c == '\\')
        }
        None => false,
    }
}

/// The `CREATE CATALOG` for `name`.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let mut sql = format!("CREATE CATALOG {}", quote_ident(Quote::Backtick, name));
    if let Some(l) = opt(o, "managed_location") {
        if !storage_url(l) {
            return Err(Error::Query(format!("ubicación administrada: «{l}» no es una URL de almacenamiento (s3://, abfss://, gs://…)")));
        }
        sql.push_str(&format!("\nMANAGED LOCATION {}", lit(l)));
    }
    if let Some(c) = opt(o, "comment") {
        sql.push_str(&format!("\nCOMMENT {}", lit(c)));
    }
    Ok(sql)
}

impl DatabricksSession {
    /// The external locations the user can see, as places to put the
    /// catalog's managed storage.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let urls = self
            .named_rows("SHOW EXTERNAL LOCATIONS")
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|mut r| r.remove("url"))
            .collect();
        Ok(vec![FieldChoices { key: "managed_location".into(), default: None, values: urls }])
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        let sql = script(name, o)?;
        self.run(&sql, 1, None).await.map(|_| ())
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
        assert_eq!(script("ventas", &o(&[])).unwrap(), "CREATE CATALOG `ventas`");
        assert_eq!(script("ven`tas", &o(&[("comment", " ")])).unwrap(), "CREATE CATALOG `ven``tas`");
    }

    #[test]
    fn location_and_comment() {
        assert_eq!(
            script("v", &o(&[("managed_location", "abfss://data@acct.dfs.core.windows.net/v"), ("comment", "it's \\ ok")])).unwrap(),
            "CREATE CATALOG `v`\nMANAGED LOCATION 'abfss://data@acct.dfs.core.windows.net/v'\nCOMMENT 'it\\'s \\\\ ok'"
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in ["/dbfs/tmp", "s3://", "S3://x", "s3://b/x' --", "s3://b\\x", "://x"] {
            assert!(script("v", &o(&[("managed_location", bad)])).is_err(), "{bad}");
        }
    }
}
